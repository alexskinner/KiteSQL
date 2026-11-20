// Copyright 2024 KipData/KiteSQL
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(all(
    test,
    not(feature = "async"),
    feature = "lmdb",
    feature = "parser",
    not(target_arch = "wasm32")
))]
mod catalog_generation_tests;
mod program;
#[cfg(feature = "parser")]
mod session;
#[cfg(feature = "parser")]
pub use crate::binder::Statement;
use crate::binder::{Binder, BinderContext};
use crate::errors::DatabaseError;
pub use crate::execution::Executor;
use crate::execution::{DDLApply, ExecArena, ExecArenaLocalState, ExecBuffer};
use crate::expression::function::scala::ScalarFunctionImpl;
use crate::expression::function::table::{
    ArcTableFunctionImpl, TableFunctionCatalog, TableFunctionImpl,
};
use crate::expression::function::FunctionSummary;
use crate::function::core::install_core_functions;
use crate::function::registry::FunctionRegistry;
use crate::maybe_async::MaybeSend;
use crate::optimizer::core::hll;
use crate::optimizer::core::statistics_meta::StatisticMetaLoader;
use crate::optimizer::heuristic::batch::HepBatchStrategy;
use crate::optimizer::heuristic::optimizer::HepOptimizerPipeline;
use crate::optimizer::rule::implementation::ImplementationRuleImpl;
use crate::optimizer::rule::normalization::NormalizationRuleImpl;
#[cfg(feature = "orm")]
use crate::orm::{FromQueryRow, FromTxnRow};
use crate::planner::operator::Operator;
use crate::planner::{ExecMetaArena, LogicalPlan, MetaArena, PlanArena, TableArenaCell};
#[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
use crate::storage::lmdb::{LmdbConfig, LmdbStorage};
#[cfg(not(feature = "async"))]
use crate::storage::memory::MemoryStorage;
#[cfg(all(not(target_arch = "wasm32"), feature = "rocksdb"))]
use crate::storage::rocksdb::{OptimisticRocksStorage, RocksStorage, StorageConfig};
use crate::storage::table_codec::TableCodec;
use crate::storage::{
    CheckpointableStorage, NameMapping, StatisticsMetaCache, Storage, StorageTransaction,
    TableCache, TransactionIsolationLevel,
};
use crate::types::tuple::{Schema, SchemaView, TupleImpl};
use crate::types::value::DataValue;
use crate::types::LogicalType;
use ordered_float::OrderedFloat;
pub use program::{Param, Program, ProgramIter};
#[cfg(feature = "parser")]
pub use session::Session;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Metadata arena, and the executor with the arena it runs in.
pub(crate) struct OwnedPtr<T>(*mut T);

// SAFETY: `OwnedPtr` owns its value like `Box<T>`.
unsafe impl<T: Send> Send for OwnedPtr<T> {}
// SAFETY: as for `Send`.
unsafe impl<T: Sync> Sync for OwnedPtr<T> {}

impl<T> OwnedPtr<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Box::into_raw(Box::new(value)))
    }

    pub(crate) fn get(&self) -> *mut T {
        self.0
    }

    /// # Safety
    /// No reference obtained through [`get`](Self::get) may still be in use.
    pub(crate) unsafe fn into_inner(self) -> T {
        let this = std::mem::ManuallyDrop::new(self);
        *Box::from_raw(this.0)
    }
}

impl<T> Drop for OwnedPtr<T> {
    fn drop(&mut self) {
        // SAFETY: `self.0` comes from `Box::into_raw` and is freed only here or in `into_inner`.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

pub(crate) type Execution<'a, 'p, A, T> = (ExecMetaArena<A>, (Executor<'p>, ExecArena<'p, 'a, T>));

pub(crate) type ScalaFunctions = HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>>;
pub(crate) type TableFunctions = HashMap<FunctionSummary, TableFunctionCatalog>;

struct BuildParams {
    histogram_buckets: Option<usize>,
    hll_precision: Option<u8>,
    statistics_relative_error: Option<f64>,
    parser_recursion_limit: Option<usize>,
    #[cfg(feature = "parser")]
    dialect_registry: crate::dialect::DialectRegistry,
    transaction_isolation: Option<TransactionIsolationLevel>,
    scalar_functions: Vec<Arc<dyn ScalarFunctionImpl>>,
    table_functions: Vec<Arc<dyn TableFunctionImpl>>,
    aggregate_functions: Vec<Arc<dyn crate::expression::agg_function::AggregateFunctionImpl>>,
    function_packs: Vec<Arc<dyn crate::function::pack::FunctionPack>>,
}

pub enum CatalogKind {
    Table(crate::catalog::TableName),
    View(crate::catalog::TableName),
    ScalarFunction(Arc<dyn ScalarFunctionImpl>),
    TableFunction(Arc<dyn TableFunctionImpl>),
}

#[maybe_async]
pub(crate) trait BindSource {
    type Iter: ResultIter;
    type Transaction: StorageTransaction;

    type Storage: Storage;

    /// `read_only` lets a fresh transaction use the storage's read path; executors
    /// that already hold a transaction ignore it.
    async fn execute<F>(self, read_only: bool, build: F) -> Result<Self::Iter, DatabaseError>
    where
        F: for<'s> FnOnce(
                &'s State<Self::Storage>,
                CatalogView<'s>,
                &Self::Transaction,
            ) -> Result<(Executor<'s>, PlanArena<'s>), DatabaseError>
            + MaybeSend;

    #[cfg(feature = "orm")]
    fn explain<A, F>(self, params: A, build: F) -> Result<String, DatabaseError>
    where
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'bind> FnOnce(
            &mut Binder<'bind, '_, Self::Transaction, A>,
            &mut PlanArena<'_>,
        ) -> Result<LogicalPlan, DatabaseError>;
}

/// Builder for creating a [`Database`] instance.
///
/// The builder wires together storage, built-in functions and optional runtime
/// features before the database is opened.
pub struct DataBaseBuilder {
    #[cfg(all(
        not(target_arch = "wasm32"),
        any(feature = "rocksdb", feature = "lmdb")
    ))]
    path: PathBuf,
    histogram_buckets: Option<usize>,
    hll_precision: Option<u8>,
    statistics_relative_error: Option<f64>,
    parser_recursion_limit: Option<usize>,
    transaction_isolation: Option<TransactionIsolationLevel>,
    #[cfg(feature = "parser")]
    dialect_registry: crate::dialect::DialectRegistry,
    #[cfg(all(not(target_arch = "wasm32"), feature = "rocksdb"))]
    storage_config: StorageConfig,
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    lmdb_config: LmdbConfig,
    scalar_functions: Vec<Arc<dyn ScalarFunctionImpl>>,
    table_functions: Vec<Arc<dyn TableFunctionImpl>>,
    aggregate_functions: Vec<Arc<dyn crate::expression::agg_function::AggregateFunctionImpl>>,
    function_packs: Vec<Arc<dyn crate::function::pack::FunctionPack>>,
}

impl DataBaseBuilder {
    /// Creates a builder rooted at the given database path.
    ///
    /// Built-in scalar functions and table functions are registered
    /// automatically.
    pub fn path(path: impl Into<PathBuf> + Send) -> Self {
        #[cfg(all(
            not(target_arch = "wasm32"),
            any(feature = "rocksdb", feature = "lmdb")
        ))]
        let path = path.into();
        #[cfg(not(all(
            not(target_arch = "wasm32"),
            any(feature = "rocksdb", feature = "lmdb")
        )))]
        let _ = path;

        DataBaseBuilder {
            #[cfg(all(
                not(target_arch = "wasm32"),
                any(feature = "rocksdb", feature = "lmdb")
            ))]
            path,
            histogram_buckets: None,
            hll_precision: None,
            statistics_relative_error: None,
            parser_recursion_limit: None,
            transaction_isolation: None,
            #[cfg(feature = "parser")]
            dialect_registry: crate::dialect::DialectRegistry::new(),
            #[cfg(all(not(target_arch = "wasm32"), feature = "rocksdb"))]
            storage_config: Default::default(),
            #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
            lmdb_config: Default::default(),
            scalar_functions: Vec::new(),
            table_functions: Vec::new(),
            aggregate_functions: Vec::new(),
            function_packs: Vec::new(),
        }
    }

    /// Sets the default histogram bucket count used by `ANALYZE`.
    pub fn histogram_buckets(mut self, buckets: usize) -> Self {
        self.histogram_buckets = Some(buckets);
        self
    }

    /// Sets the HyperLogLog precision `p` used by `ANALYZE` to estimate column NDV.
    ///
    /// Each sketch has `2^p` registers (one byte each) with a standard error of
    /// about `1.04 / sqrt(2^p)`. Valid range is 4..=26; unset uses 14
    /// (16 KiB, ~0.81% error).
    pub fn hll_precision(mut self, precision: u8) -> Self {
        self.hll_precision = Some(precision);
        self
    }

    /// Sets the relative error `ε` of the quantile (KLL) and frequency
    /// (Count-Min) sketches built by `ANALYZE`.
    ///
    /// Sketch size grows roughly as `1 / ε` (Count-Min width is
    /// `next_power_of_two(2 / ε)`). Must be in `(0, 1)`; unset uses 0.001.
    pub fn statistics_relative_error(mut self, relative_error: f64) -> Self {
        self.statistics_relative_error = Some(relative_error);
        self
    }

    /// Sets the SQL parser recursion limit. Unset uses the parser's default limit.
    /// Higher limits may require a larger thread stack for deeply nested SQL.
    #[cfg(feature = "parser")]
    pub fn parser_recursion_limit(mut self, limit: usize) -> Self {
        self.parser_recursion_limit = Some(limit);
        self
    }

    /// Registers a SQL dialect.
    #[cfg(feature = "parser")]
    pub fn register_dialect(mut self, dialect: Arc<dyn crate::dialect::SqlDialect>) -> Self {
        self.dialect_registry.register(dialect);
        self
    }

    /// Sets the default SQL dialect. The default is "generic".
    #[cfg(feature = "parser")]
    pub fn default_dialect(mut self, name: &str) -> Result<Self, DatabaseError> {
        self.dialect_registry.set_default(name)?;
        Ok(self)
    }

    /// Sets the transaction isolation level used by database-created transactions.
    pub fn transaction_isolation(mut self, isolation: TransactionIsolationLevel) -> Self {
        self.transaction_isolation = Some(isolation);
        self
    }

    /// Register a scalar function that will be available in the database.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let db = DataBaseBuilder::path("./data")
    ///     .register_scalar_function(MyFunction::new())
    ///     .build()?;
    /// ```
    pub fn register_scalar_function(mut self, function: Arc<dyn ScalarFunctionImpl>) -> Self {
        self.scalar_functions.push(function);
        self
    }

    /// Register a table function that will be available in the database.
    pub fn register_table_function(mut self, function: Arc<dyn TableFunctionImpl>) -> Self {
        self.table_functions.push(function);
        self
    }

    /// Register an aggregate function that will be available in the database.
    ///
    /// A registered aggregate function with the same name as a built-in aggregate (e.g. `sum`, `count`)
    /// overrides the built-in, following the same policy as scalar functions.
    pub fn register_aggregate_function(
        mut self,
        function: Arc<dyn crate::expression::agg_function::AggregateFunctionImpl>,
    ) -> Self {
        self.aggregate_functions.push(function);
        self
    }

    /// Install a function pack, adding all functions from the pack to the database.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let db = DataBaseBuilder::path("./data")
    ///     .with_function_pack(Arc::new(MyDialectFunctions))
    ///     .build()?;
    /// ```
    pub fn with_function_pack(
        mut self,
        pack: Arc<dyn crate::function::pack::FunctionPack>,
    ) -> Self {
        self.function_packs.push(pack);
        self
    }

    /// Enables or disables RocksDB statistics collection.
    #[cfg(all(
        not(target_arch = "wasm32"),
        any(feature = "rocksdb", feature = "lmdb")
    ))]
    pub fn storage_statistics(mut self, enable: bool) -> Self {
        #[cfg(feature = "rocksdb")]
        {
            self.storage_config.enable_statistics = enable;
        }
        #[cfg(feature = "lmdb")]
        {
            self.lmdb_config.enable_statistics = enable;
        }
        self
    }

    #[cfg(all(
        not(target_arch = "wasm32"),
        any(feature = "rocksdb", feature = "lmdb")
    ))]
    pub fn sync(mut self, enable: bool) -> Self {
        #[cfg(feature = "rocksdb")]
        {
            self.storage_config.sync = enable;
        }
        #[cfg(feature = "lmdb")]
        {
            self.lmdb_config
                .flags
                .set(lmdb::EnvironmentFlags::NO_SYNC, !enable);
        }
        self
    }

    /// Sets the LMDB map size in bytes.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn lmdb_map_size(mut self, map_size: usize) -> Self {
        self.lmdb_config.map_size = map_size;
        self
    }

    /// Sets the LMDB environment flags.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn lmdb_flags(mut self, flags: lmdb::EnvironmentFlags) -> Self {
        self.lmdb_config.flags = flags;
        self
    }

    /// Enables or disables LMDB `NO_SYNC`.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn lmdb_no_sync(mut self, enable: bool) -> Self {
        self.lmdb_config
            .flags
            .set(lmdb::EnvironmentFlags::NO_SYNC, enable);
        self
    }

    /// Sets the maximum number of LMDB readers.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn lmdb_max_readers(mut self, max_readers: u32) -> Self {
        self.lmdb_config.max_readers = Some(max_readers);
        self
    }

    /// Sets the maximum number of LMDB named databases.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn lmdb_max_dbs(mut self, max_dbs: u32) -> Self {
        self.lmdb_config.max_dbs = Some(max_dbs);
        self
    }

    /// Builds a database using a custom storage implementation.
    pub fn build_with_storage<T: Storage>(self, storage: T) -> Result<Database<T>, DatabaseError> {
        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    /// Builds a database for the current target platform.
    #[cfg(all(target_arch = "wasm32", feature = "wasm"))]
    pub fn build(self) -> Result<Database<MemoryStorage>, DatabaseError> {
        let storage = MemoryStorage::new();

        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    /// Builds a RocksDB-backed database.
    #[cfg(all(not(target_arch = "wasm32"), feature = "rocksdb"))]
    pub fn build_rocksdb(self) -> Result<Database<RocksStorage>, DatabaseError> {
        let storage = RocksStorage::with_config(self.path, self.storage_config)?;

        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    /// Builds an in-memory database.
    ///
    /// This is convenient for tests, examples and temporary workloads.
    #[cfg(not(feature = "async"))]
    pub fn build_in_memory(self) -> Result<Database<MemoryStorage>, DatabaseError> {
        let storage = MemoryStorage::new();

        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    /// Builds a LMDB-backed database.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
    pub fn build_lmdb(self) -> Result<Database<LmdbStorage>, DatabaseError> {
        let storage = LmdbStorage::with_config(self.path, self.lmdb_config)?;

        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "rocksdb"))]
    /// Builds a RocksDB-backed database that uses optimistic transactions.
    pub fn build_optimistic(self) -> Result<Database<OptimisticRocksStorage>, DatabaseError> {
        let storage = OptimisticRocksStorage::with_config(self.path, self.storage_config)?;

        Self::_build(
            storage,
            BuildParams {
                histogram_buckets: self.histogram_buckets,
                hll_precision: self.hll_precision,
                statistics_relative_error: self.statistics_relative_error,
                parser_recursion_limit: self.parser_recursion_limit,
                #[cfg(feature = "parser")]
                dialect_registry: self.dialect_registry,
                transaction_isolation: self.transaction_isolation,
                scalar_functions: self.scalar_functions,
                table_functions: self.table_functions,
                aggregate_functions: self.aggregate_functions,
                function_packs: self.function_packs,
            },
        )
    }

    fn _build<T: Storage>(storage: T, params: BuildParams) -> Result<Database<T>, DatabaseError> {
        let BuildParams {
            histogram_buckets,
            hll_precision,
            statistics_relative_error,
            parser_recursion_limit,
            #[cfg(feature = "parser")]
            dialect_registry,
            transaction_isolation,
            scalar_functions,
            table_functions,
            aggregate_functions,
            function_packs,
        } = params;

        if matches!(histogram_buckets, Some(0)) {
            return Err(DatabaseError::InvalidValue(
                "histogram buckets must be >= 1".to_string(),
            ));
        }
        // Reject out-of-range values when the database opens, not at the first ANALYZE.
        if let Some(precision) = hll_precision {
            if !(hll::MIN_PRECISION..=hll::MAX_PRECISION).contains(&precision) {
                return Err(DatabaseError::InvalidValue(format!(
                    "hll precision must be between {} and {}, got {precision}",
                    hll::MIN_PRECISION,
                    hll::MAX_PRECISION
                )));
            }
        }
        if let Some(relative_error) = statistics_relative_error {
            if !(relative_error > 0.0 && relative_error < 1.0) {
                return Err(DatabaseError::InvalidValue(format!(
                    "statistics relative error must be in (0, 1), got {relative_error}"
                )));
            }
        }
        if matches!(parser_recursion_limit, Some(0)) {
            return Err(DatabaseError::InvalidValue(
                "parser recursion limit must be >= 1".to_string(),
            ));
        }
        let transaction_isolation =
            transaction_isolation.unwrap_or_else(|| storage.default_transaction_isolation());
        storage.validate_transaction_isolation(transaction_isolation)?;
        let meta_cache = StatisticsMetaCache::default();
        let table_arena = TableArenaCell::default();

        let mut function_registry = FunctionRegistry::new();
        install_core_functions(&mut function_registry, table_arena.borrow_mut())?;

        function_registry.mark_builtins_loaded();

        for function in scalar_functions {
            function_registry.register_scalar(function)?;
        }

        for function in table_functions {
            let mut schema = Schema::new();
            function.output_schema_into(table_arena.borrow_mut(), &mut schema);
            let catalog = TableFunctionCatalog {
                inner: ArcTableFunctionImpl(function),
                schema,
            };
            function_registry.register_table(Arc::new(catalog))?;
        }

        for function in aggregate_functions {
            function_registry.register_aggregate(function)?;
        }

        for pack in function_packs {
            pack.install(&mut function_registry)?;
        }

        #[cfg(feature = "parser")]
        let default_dialect = dialect_registry.default_dialect();

        // Generation 0: the base catalog, published like every later generation.
        let live_catalogs = LiveCatalogs::default();
        let generation_0 = Arc::new(PublishedCatalog::new(
            table_arena,
            meta_cache,
            0,
            &live_catalogs,
        ));
        let placeholder = Arc::new(PublishedCatalog::new(
            TableArenaCell::default(),
            StatisticsMetaCache::default(),
            0,
            &live_catalogs,
        ));
        let state = State {
            function_registry,
            published: arc_swap::ArcSwap::new(generation_0),
            generation_counter: std::sync::atomic::AtomicU64::new(0),
            retained: std::sync::OnceLock::new(),
            placeholder,
            live_catalogs,
            optimizer_pipeline: optimizer_pipeline(),
            histogram_buckets,
            hll_precision,
            statistics_relative_error: statistics_relative_error.map(OrderedFloat),
            parser_recursion_limit,
            #[cfg(feature = "parser")]
            dialect_registry,
            #[cfg(feature = "parser")]
            default_dialect,
            _p: Default::default(),
        };

        Ok(Database {
            storage,
            transaction_isolation,
            state,
            id: Arc::new(()),
        })
    }
}

fn optimizer_pipeline() -> HepOptimizerPipeline {
    HepOptimizerPipeline::builder()
        .before_batch(
            "Column Pruning",
            HepBatchStrategy::once_topdown(),
            &[NormalizationRuleImpl::ColumnPruning],
        )
        .before_batch(
            "Simplify Filter",
            HepBatchStrategy::fix_point_topdown(10),
            &[NormalizationRuleImpl::SimplifyFilter],
        )
        .before_batch(
            "Constant Calculation",
            HepBatchStrategy::once_topdown(),
            &[NormalizationRuleImpl::ConstantCalculation],
        )
        .before_batch(
            "Predicate Pushdown",
            HepBatchStrategy::fix_point_topdown(30),
            &[
                NormalizationRuleImpl::PropagateFilterConstants,
                NormalizationRuleImpl::PropagateJoinConstants,
                NormalizationRuleImpl::PushPredicateThroughJoin,
                NormalizationRuleImpl::PushJoinPredicateIntoScan,
            ],
        )
        .before_batch(
            "Limit Pushdown",
            HepBatchStrategy::fix_point_topdown(10),
            &[
                NormalizationRuleImpl::LimitProjectTranspose,
                NormalizationRuleImpl::PushLimitThroughJoin,
            ],
        )
        .before_batch(
            "TopK",
            HepBatchStrategy::fix_point_topdown(1),
            &[
                NormalizationRuleImpl::MinMaxToTopK,
                NormalizationRuleImpl::TopK,
            ],
        )
        .before_batch(
            "Collapse Project",
            HepBatchStrategy::fix_point_bottomup(10),
            &[NormalizationRuleImpl::EliminateProject],
        )
        .before_batch(
            "Combine Operators",
            HepBatchStrategy::fix_point_topdown(10),
            &[
                NormalizationRuleImpl::CollapseGroupByAgg,
                NormalizationRuleImpl::CombineFilter,
            ],
        )
        .before_batch(
            "Predicate Into Scan",
            HepBatchStrategy::fix_point_topdown(10),
            &[NormalizationRuleImpl::PushPredicateIntoScan],
        )
        .before_batch(
            "Guard Mark Apply",
            HepBatchStrategy::fix_point_topdown(1),
            &[NormalizationRuleImpl::GuardMarkApply],
        )
        .after_batch(
            "Greedy Join Reorder",
            HepBatchStrategy::once_topdown(),
            &[NormalizationRuleImpl::GreedyJoinReorder],
        )
        .after_batch(
            "Parameterize Mark Apply",
            HepBatchStrategy::fix_point_topdown(1),
            &[
                NormalizationRuleImpl::ParameterizeMarkApply,
                NormalizationRuleImpl::ParameterizeInnerJoin,
            ],
        )
        // Only discard predicates after parameterization has chosen the final
        // lookup. A Probe must not inherit residuals from a replaced static range.
        .after_batch(
            "Eliminate Index Filter",
            HepBatchStrategy::fix_point_topdown(1),
            &[NormalizationRuleImpl::EliminateIndexFilter],
        )
        .after_batch(
            "Materialize Mark Apply",
            HepBatchStrategy::fix_point_bottomup(1),
            &[NormalizationRuleImpl::MaterializeMarkApply],
        )
        .after_batch(
            "Collapse Project",
            HepBatchStrategy::fix_point_bottomup(10),
            &[NormalizationRuleImpl::EliminateProject],
        )
        .after_batch(
            "Expression Remapper",
            HepBatchStrategy::once_topdown(),
            &[NormalizationRuleImpl::EvaluatorBind],
        )
        .after_batch(
            "Limit Into Scan",
            HepBatchStrategy::fix_point_topdown(10),
            &[NormalizationRuleImpl::PushLimitIntoTableScan],
        )
        .after_batch(
            "Skip Residual Filter Columns",
            HepBatchStrategy::once_topdown(),
            &[NormalizationRuleImpl::SkipResidualFilterColumns],
        )
        .after_batch(
            "Map Project Columns",
            HepBatchStrategy::fix_point_topdown(1),
            &[NormalizationRuleImpl::MapProjectColumns],
        )
        .implementations(&[
            // DQL
            ImplementationRuleImpl::SimpleAggregate,
            ImplementationRuleImpl::GroupByAggregate,
            ImplementationRuleImpl::Dummy,
            ImplementationRuleImpl::Filter,
            ImplementationRuleImpl::HashJoin,
            ImplementationRuleImpl::Limit,
            ImplementationRuleImpl::Projection,
            ImplementationRuleImpl::MarkApply,
            ImplementationRuleImpl::ScalarSubquery,
            ImplementationRuleImpl::SeqScan,
            ImplementationRuleImpl::IndexScan,
            ImplementationRuleImpl::FunctionScan,
            ImplementationRuleImpl::Sort,
            ImplementationRuleImpl::TopK,
            ImplementationRuleImpl::Values,
            ImplementationRuleImpl::Window,
            // DML
            ImplementationRuleImpl::Analyze,
            #[cfg(feature = "copy")]
            ImplementationRuleImpl::CopyFromFile,
            #[cfg(feature = "copy")]
            ImplementationRuleImpl::CopyToFile,
            ImplementationRuleImpl::Delete,
            ImplementationRuleImpl::Insert,
            ImplementationRuleImpl::Update,
            // DDL
            ImplementationRuleImpl::AddColumn,
            ImplementationRuleImpl::ChangeColumn,
            ImplementationRuleImpl::CreateTable,
            ImplementationRuleImpl::DropColumn,
            ImplementationRuleImpl::DropTable,
            ImplementationRuleImpl::Truncate,
        ])
        .build()
}

fn statement_stamp<T: StorageTransaction>(
    transaction: &mut T,
    plan: &LogicalPlan,
) -> Result<u64, DatabaseError> {
    let scans_and_writes = match &plan.operator {
        Operator::Insert(_) => !matches!(plan.childrens.only().operator, Operator::Values(_)),
        Operator::Update(_)
        | Operator::AddColumn(_)
        | Operator::ChangeColumn(_)
        | Operator::DropColumn(_)
        | Operator::CreateIndex(_)
        | Operator::Analyze(_) => true,
        _ => false,
    };
    if scans_and_writes {
        transaction.next_statement_stamp()
    } else {
        Ok(0)
    }
}

#[maybe_async]
async fn start_execution<'a, 'p, T: StorageTransaction + 'p, A: MetaArena + 'p>(
    table_cache: &'p TableCache,
    transaction: &'a mut T,
    mut executor: Executor<'p>,
    plan_arena: A,
    function_registry: Option<&'p crate::function::registry::FunctionRegistry>,
    enable_undo: bool,
) -> Result<Execution<'a, 'p, A, T>, DatabaseError>
where
    'p: 'a,
{
    let mut plan_arena = ExecMetaArena::new(plan_arena);
    let arena = start_execution_inner(
        table_cache,
        transaction,
        &mut executor,
        &mut plan_arena,
        function_registry,
        enable_undo,
    )
    .await?;

    Ok((plan_arena, (executor, arena)))
}

#[maybe_async]
async fn start_execution_inner<'a, 'p, T: StorageTransaction + 'p, A: MetaArena + 'p>(
    table_cache: &'p TableCache,
    transaction: &'a mut T,
    executor: &mut Executor<'p>,
    plan_arena: &mut ExecMetaArena<A>,
    function_registry: Option<&'p crate::function::registry::FunctionRegistry>,
    enable_statement_undo: bool,
) -> Result<ExecArena<'p, 'a, T>, DatabaseError>
where
    'p: 'a,
{
    let stamp = statement_stamp(transaction, executor.plan())?;
    let iters = executor.buffer.iter_slots(executor.iters());
    let mut arena = ExecArena::new(table_cache, transaction)
        .with_iters(iters)
        .with_statement_undo(enable_statement_undo);
    if let Some(registry) = function_registry {
        arena = arena.with_function_registry(registry);
    }
    executor.buffer.set_statement_stamp(stamp);
    executor.start(&mut arena, plan_arena).await?;

    Ok(arena)
}

/// A borrowed view of one catalog generation: the table arena and the statistics cache that
/// belong to it. Plans borrow a view for as long as they live; the owner of the view (a
/// [`CatalogRef`], a [`PrivateCatalog`] or a retained generation) keeps it alive.
#[derive(Clone, Copy)]
pub(crate) struct CatalogView<'a> {
    pub(crate) arena: &'a TableArenaCell,
    pub(crate) meta: &'a StatisticsMetaCache,
}

/// An owned handle on one published catalog generation.
///
/// Holding a `CatalogRef` keeps its generation alive; the generation is freed when the last
/// handle (and the database's own pointer, once a newer generation replaces it) is dropped.
/// Accessors borrow from `&self`, so nothing can outlive the handle.
#[derive(Clone)]
pub(crate) struct CatalogRef(Arc<PublishedCatalog>);

impl CatalogRef {
    pub(crate) fn generation(&self) -> u64 {
        self.0.generation
    }

    pub(crate) fn arena(&self) -> &TableArenaCell {
        &self.0.arena
    }

    pub(crate) fn view(&self) -> CatalogView<'_> {
        self.0.view()
    }

    #[cfg(test)]
    pub(crate) fn meta(&self) -> &StatisticsMetaCache {
        &self.0.meta
    }
}

/// Counts a database's live [`PublishedCatalog`]s in tests; empty otherwise.
#[derive(Clone, Default)]
pub(crate) struct LiveCatalogs {
    #[cfg(test)]
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl LiveCatalogs {
    fn token(&self) -> LiveCatalogToken {
        #[cfg(test)]
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LiveCatalogToken {
            #[cfg(test)]
            count: self.count.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Held by each [`PublishedCatalog`]; gives its count back on drop (in tests).
struct LiveCatalogToken {
    #[cfg(test)]
    count: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl Drop for LiveCatalogToken {
    fn drop(&mut self) {
        self.count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One catalog generation.
///
/// Generation 0 is built from the database's base catalog when it opens; every later one is
/// either rebuilt from committed storage by a `Session` commit ([`Database::publish_catalog`])
/// or produced by a `&mut Database` entry point. All of them live in [`State::published`] and
/// are reference counted, so a generation is freed as soon as nothing pins it.
pub(crate) struct PublishedCatalog {
    arena: TableArenaCell,
    meta: StatisticsMetaCache,
    /// Strictly increasing across every generation this database stores.
    generation: u64,
    _live: LiveCatalogToken,
}

impl PublishedCatalog {
    fn new(
        arena: TableArenaCell,
        meta: StatisticsMetaCache,
        generation: u64,
        live: &LiveCatalogs,
    ) -> Self {
        Self {
            arena,
            meta,
            generation,
            _live: live.token(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn view(&self) -> CatalogView<'_> {
        CatalogView {
            arena: &self.arena,
            meta: &self.meta,
        }
    }
}

fn next_generation(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// A generation taken out of [`State::published`] by a `&mut Database` entry point; see
/// [`State::edit_catalog`].
struct CatalogEdit<'s, S> {
    state: &'s State<S>,
    catalog: Option<PublishedCatalog>,
}

impl<S> CatalogEdit<'_, S> {
    /// The arena to edit in place (through its cell) and the statistics to edit.
    fn parts(&mut self) -> (&TableArenaCell, &mut StatisticsMetaCache) {
        let catalog = self
            .catalog
            .as_mut()
            .expect("catalog is taken only on drop");
        (&catalog.arena, &mut catalog.meta)
    }
}

impl<S> Drop for CatalogEdit<'_, S> {
    fn drop(&mut self) {
        if let Some(mut catalog) = self.catalog.take() {
            catalog.generation = next_generation(&self.state.generation_counter);
            self.state.published.store(Arc::new(catalog));
        }
    }
}
/// A generation lent out for the whole `&Database` borrow, as [`Program`]s need.
///
/// Append-only while the database is shared; emptied by the next `&mut Database` entry point.
/// Only generations a program was prepared against land here, each at most once.
struct RetainedCatalog {
    catalog: Arc<PublishedCatalog>,
    next: std::sync::OnceLock<Box<RetainedCatalog>>,
}

/// A catalog a session builds for itself from its own transaction's view of storage.
pub(crate) struct PrivateCatalog {
    pub(crate) arena: TableArenaCell,
    pub(crate) meta: StatisticsMetaCache,
}

impl PrivateCatalog {
    pub(crate) fn as_ref(&self) -> CatalogView<'_> {
        CatalogView {
            arena: &self.arena,
            meta: &self.meta,
        }
    }
}

pub(crate) struct State<S> {
    function_registry: FunctionRegistry,
    /// The newest catalog generation. Readers pin it by cloning the `Arc`.
    published: arc_swap::ArcSwap<PublishedCatalog>,
    /// Source of generation numbers; only ever increases.
    generation_counter: std::sync::atomic::AtomicU64,
    /// Generations lent to programs for the whole `&Database` borrow. See [`RetainedCatalog`].
    retained: std::sync::OnceLock<Box<RetainedCatalog>>,
    /// Empty catalog parked in `published` while a `&mut` entry point swaps generations.
    /// Never visible to a reader.
    placeholder: Arc<PublishedCatalog>,
    live_catalogs: LiveCatalogs,
    optimizer_pipeline: HepOptimizerPipeline,
    histogram_buckets: Option<usize>,
    hll_precision: Option<u8>,
    statistics_relative_error: Option<OrderedFloat<f64>>,
    #[cfg_attr(not(feature = "parser"), allow(dead_code))]
    parser_recursion_limit: Option<usize>,
    #[cfg(feature = "parser")]
    dialect_registry: crate::dialect::DialectRegistry,
    #[cfg(feature = "parser")]
    default_dialect: Arc<dyn crate::dialect::SqlDialect>,
    _p: PhantomData<S>,
}

impl<S: Storage> State<S> {
    #[cfg(feature = "parser")]
    pub(crate) fn parser_recursion_limit(&self) -> Option<usize> {
        self.parser_recursion_limit
    }

    #[cfg(feature = "parser")]
    pub(crate) fn dialect_registry(&self) -> &crate::dialect::DialectRegistry {
        &self.dialect_registry
    }

    #[cfg(feature = "parser")]
    pub(crate) fn dialect(&self) -> &dyn crate::dialect::SqlDialect {
        self.default_dialect.as_ref()
    }

    pub(crate) fn function_registry(&self) -> &FunctionRegistry {
        &self.function_registry
    }

    pub(crate) fn scala_functions_arc(&self) -> Arc<ScalaFunctions> {
        Arc::new(self.function_registry.scalar_functions().clone())
    }

    pub(crate) fn table_functions_arc(&self) -> Arc<TableFunctions> {
        Arc::new(
            self.function_registry
                .table_functions()
                .iter()
                .map(|(k, v)| (k.clone(), (**v).clone()))
                .collect(),
        )
    }
    /// Pins the newest catalog generation.
    pub(crate) fn catalog(&self) -> CatalogRef {
        CatalogRef(self.published.load_full())
    }

    fn next_generation(&self) -> u64 {
        next_generation(&self.generation_counter)
    }

    /// Lends `catalog`'s generation for as long as `self` is borrowed, for plans (programs)
    /// that must outlive any handle the caller could keep. Each generation is retained at
    /// most once, until the next `&mut Database` entry point.
    pub(crate) fn retain(&self, catalog: &CatalogRef) -> &PublishedCatalog {
        let mut slot = &self.retained;
        loop {
            match slot.get() {
                Some(node) => {
                    if node.catalog.generation == catalog.generation() {
                        return &node.catalog;
                    }
                    slot = &node.next;
                }
                None => {
                    // Lost a race to another retainer: keep walking from what it appended.
                    let _ = slot.set(Box::new(RetainedCatalog {
                        catalog: catalog.0.clone(),
                        next: std::sync::OnceLock::new(),
                    }));
                }
            }
        }
    }

    /// Drops every retained generation. Needs `&mut`, so no program can still borrow one.
    pub(crate) fn release_retained(&mut self) {
        // Unlinked one node at a time, so a long chain does not recurse on drop.
        let mut next = self.retained.take();
        while let Some(mut node) = next {
            next = node.next.take();
        }
    }

    /// Takes the newest generation out for a `&mut Database` entry point to edit in place.
    /// The edited generation is published again, under a new number, when the guard drops,
    /// on every path (errors and cancelled futures included).
    ///
    /// Only for callers holding `&mut Database` that ran [`State::release_retained`] first;
    /// it takes `&self` so the state can still be read while the guard is alive.
    fn edit_catalog(&self) -> CatalogEdit<'_, S> {
        let current = self.published.swap(self.placeholder.clone());
        // With `&mut self` no transaction, iterator or program can pin a generation, so this
        // is the only pointer left; copy it if that ever stops being true.
        let catalog = Arc::try_unwrap(current).unwrap_or_else(|shared| {
            PublishedCatalog::new(
                TableArenaCell::new(shared.arena.borrow().clone()),
                StatisticsMetaCache {
                    indexes: shared.meta.indexes.clone(),
                    columns: shared.meta.columns.clone(),
                },
                shared.generation,
                &self.live_catalogs,
            )
        });
        CatalogEdit {
            state: self,
            catalog: Some(catalog),
        }
    }

    fn load_scalar_function(&mut self, function: Arc<dyn ScalarFunctionImpl>) {
        self.function_registry.load_scalar(function);
    }

    fn load_table_function(
        &mut self,
        function: Arc<dyn TableFunctionImpl>,
    ) -> Result<(), DatabaseError> {
        self.release_retained();
        let mut edit = self.edit_catalog();
        let (arena, _) = edit.parts();
        let mut schema = Schema::new();
        function.output_schema_into(arena.borrow_mut(), &mut schema);
        let table_function = TableFunctionCatalog {
            inner: ArcTableFunctionImpl(function),
            schema,
        };
        drop(edit);
        self.function_registry.load_table(Arc::new(table_function));
        let mut edit = self.edit_catalog();
        let (arena, _) = edit.parts();
        Self::recycle_arena(arena, &self.function_registry)
    }

    fn recycle_arena(
        arena: &TableArenaCell,
        function_registry: &FunctionRegistry,
    ) -> Result<(), DatabaseError> {
        let mut live_columns = HashSet::new();
        {
            let mut live = |column: &crate::catalog::ColumnRef| {
                live_columns.insert(column.pos());
            };

            let table_arena = arena.borrow();
            for table in table_arena.tables.values() {
                for column in table.columns() {
                    live(column);
                }
            }
            for view in table_arena.views.values() {
                view.visit_column_refs(table_arena, &mut live)?;
            }

            for function in function_registry.table_functions().values() {
                for column in &function.schema {
                    live(column);
                }
            }
        }
        arena
            .borrow_mut()
            .recycle_unreferenced_positions(live_columns);
        Ok(())
    }

    /// Binds and optimizes against an explicit catalog generation, so one statement never
    /// mixes two generations (the shared latest one, or a session's private one).
    pub(crate) fn build_plan_in<'a, T: StorageTransaction, A: AsRef<[(usize, LogicalType)]>, F>(
        &'a self,
        catalog: CatalogView<'a>,
        params: A,
        transaction: &T,
        build: F,
    ) -> Result<(LogicalPlan, PlanArena<'a>), DatabaseError>
    where
        F: for<'bind> FnOnce(
            &mut Binder<'bind, '_, T, A>,
            &mut PlanArena<'a>,
        ) -> Result<LogicalPlan, DatabaseError>,
    {
        let mut arena = PlanArena::new(catalog.arena);
        let tables = catalog.arena.borrow();
        let mut binder = Binder::new(
            BinderContext::new(
                &tables.names,
                &tables.tables,
                &tables.views,
                transaction,
                self.function_registry(),
                #[cfg(feature = "parser")]
                self.dialect(),
            ),
            &params,
            None,
        );
        let source_plan = build(&mut binder, &mut arena)?;
        drop(binder);
        let mut plan = self
            .optimizer_pipeline
            .instantiate(source_plan)
            .find_best(Some(&StatisticMetaLoader::new(catalog.meta)), &mut arena)?;

        if let Operator::Analyze(op) = &mut plan.operator {
            if op.histogram_buckets.is_none() {
                op.histogram_buckets = self.histogram_buckets;
            }
            if op.hll_precision.is_none() {
                op.hll_precision = self.hll_precision;
            }
            if op.statistics_relative_error.is_none() {
                op.statistics_relative_error = self.statistics_relative_error;
            }
        }
        plan.populate_output_schema_recursive(&mut arena);

        Ok((plan, arena))
    }
}

#[maybe_async]
impl<S: Storage> State<S> {
    pub(crate) async fn execute<'a, 'p, 'txn, A: MetaArena + 'p>(
        &'p self,
        transaction: &'a mut S::TransactionType<'txn>,
        executor: Executor<'p>,
        plan_arena: A,
        enable_undo: bool,
    ) -> Result<Execution<'a, 'p, A, S::TransactionType<'txn>>, DatabaseError>
    where
        S: 'txn,
        'txn: 'p,
        'p: 'a,
    {
        // The plan's own catalog generation, never the current one: they can differ.
        let table_cache = &plan_arena.table_arena_cell().borrow().tables;
        start_execution(
            table_cache,
            transaction,
            executor,
            plan_arena,
            Some(&self.function_registry),
            enable_undo,
        )
        .await
    }

    /// Rebuilds a whole catalog from what `transaction` sees in storage, starting from a copy
    /// of `base` so positions of function schemas and other arena users stay valid.
    ///
    /// With a session's own transaction this includes its uncommitted DDL.
    pub(crate) async fn build_catalog<T: StorageTransaction>(
        &self,
        base: CatalogView<'_>,
        transaction: &T,
    ) -> Result<PrivateCatalog, DatabaseError> {
        let mut arena = base.arena.borrow().clone();
        arena.tables.clear();
        arena.views.clear();
        arena.names = NameMapping::default();
        let arena = TableArenaCell::new(arena);
        let mut meta = StatisticsMetaCache::default();
        let mut table_codec = TableCodec::default();

        let mut table_names = Vec::new();
        {
            let mut iter = transaction.tables(&mut table_codec).await?;
            while let Some(table_meta) = iter.try_next(arena.borrow_mut()).await? {
                table_names.push(table_meta.table_name);
            }
        }
        let mut view_names = Vec::new();
        {
            let mut iter = transaction.views(&mut table_codec).await?;
            while let Some(name) = iter.try_next(arena.borrow_mut()).await? {
                view_names.push(name);
            }
        }

        for name in table_names {
            let Some(table) = transaction
                .load_table(&mut table_codec, arena.borrow_mut(), &name)
                .await?
            else {
                continue;
            };
            let table_id = table.id();
            for index in table.indexes() {
                let index_id = arena.borrow().index(*index).id;
                if let Some(stats) = transaction
                    .index_statistics(&mut table_codec, table_id, index_id, arena.borrow_mut())
                    .await?
                {
                    meta.indexes.insert((table_id, index_id), stats);
                }
            }
            for column in table.columns() {
                let column_id = {
                    let arena = arena.borrow();
                    let column = arena.column(*column);
                    column
                        .id()
                        .ok_or_else(|| DatabaseError::invalid_column(column.name()))?
                };
                if let Some(stats) = transaction
                    .column_statistics(&mut table_codec, table_id, column_id, arena.borrow_mut())
                    .await?
                {
                    meta.columns.insert((table_id, column_id), stats);
                }
            }
            arena.borrow_mut().insert_table(table);
        }
        for name in view_names {
            if let Some((view_id, view)) = transaction
                .load_view(&mut table_codec, &arena, &self.function_registry, name)
                .await?
            {
                arena.borrow_mut().insert_view(view_id, view);
            }
        }
        Self::recycle_arena(&arena, &self.function_registry)?;
        Ok(PrivateCatalog { arena, meta })
    }

    pub(crate) async fn execute_mut<'a, 'txn, A, F>(
        &'a self,
        table_arena: &'a TableArenaCell,
        meta_cache: &StatisticsMetaCache,
        transaction: &'a mut S::TransactionType<'txn>,
        params: A,
        build: F,
        enable_undo: bool,
    ) -> Result<Execution<'a, 'a, PlanArena<'a>, S::TransactionType<'txn>>, DatabaseError>
    where
        S: 'txn,
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'bind> FnOnce(
            &mut Binder<'bind, '_, S::TransactionType<'txn>, A>,
            &mut PlanArena<'a>,
        ) -> Result<LogicalPlan, DatabaseError>,
    {
        let arena = table_arena.borrow();
        let table_cache = &arena.tables;
        let mut plan_arena = PlanArena::new(table_arena);
        let mut binder = Binder::new(
            BinderContext::new(
                &arena.names,
                table_cache,
                &arena.views,
                transaction,
                &self.function_registry,
                #[cfg(feature = "parser")]
                self.dialect(),
            ),
            &params,
            None,
        );
        let source_plan = build(&mut binder, &mut plan_arena)?;
        drop(binder);
        let mut plan = self
            .optimizer_pipeline
            .instantiate(source_plan)
            .find_best(Some(&StatisticMetaLoader::new(meta_cache)), &mut plan_arena)?;

        if let Operator::Analyze(op) = &mut plan.operator {
            if op.histogram_buckets.is_none() {
                op.histogram_buckets = self.histogram_buckets;
            }
            if op.hll_precision.is_none() {
                op.hll_precision = self.hll_precision;
            }
            if op.statistics_relative_error.is_none() {
                op.statistics_relative_error = self.statistics_relative_error;
            }
        }

        plan.populate_output_schema_recursive(&mut plan_arena);
        let executor = Executor::new(plan, &plan_arena);
        start_execution(
            table_cache,
            transaction,
            executor,
            plan_arena,
            Some(&self.function_registry),
            enable_undo,
        )
        .await
    }
}

/// Main database handle for executing SQL and creating transactions.
pub struct Database<S: Storage> {
    pub(crate) storage: S,
    pub(crate) transaction_isolation: TransactionIsolationLevel,
    pub(crate) state: State<S>,
    /// Unique identity for this database instance, used to detect cross-database program usage.
    pub(crate) id: Arc<()>,
}

impl DDLApply {
    fn apply_to(
        self,
        table_arena: &TableArenaCell,
        meta_cache: &mut StatisticsMetaCache,
        plan_arena: &mut PlanArena,
    ) -> Result<bool, DatabaseError> {
        let mut catalog_changed = false;
        match self {
            DDLApply::UpsertTable {
                table,
                clear_statistics,
            } => {
                let table = table.transplant_to_table_arena(plan_arena)?;
                if clear_statistics {
                    meta_cache.remove_table(table.id());
                }
                table_arena.borrow_mut().insert_table(table);
                catalog_changed = true;
            }
            DDLApply::DropTable { name } => {
                let arena = table_arena.borrow_mut();
                if let Some(meta) = arena.names.remove(&name) {
                    arena.tables.remove(&meta.id);
                    meta_cache.remove_table(meta.id);
                }
                catalog_changed = true;
            }
            DDLApply::UpsertView { id, view } => {
                plan_arena.materialize_into_table_arena();
                table_arena.borrow_mut().insert_view(id, view);
                catalog_changed = true;
            }
            DDLApply::DropView { name } => {
                let arena = table_arena.borrow_mut();
                if let Some(meta) = arena.names.remove(&name) {
                    arena.views.remove(&meta.id);
                }
                catalog_changed = true;
            }
            DDLApply::UpsertIndexStatistics {
                table_id,
                index_id,
                meta,
            } => {
                meta_cache.indexes.insert((table_id, index_id), meta);
            }
            DDLApply::RemoveIndexStatistics { table_id, index_id } => {
                meta_cache.indexes.remove(&(table_id, index_id));
            }
            DDLApply::UpsertColumnStatistics {
                table_id,
                column_id,
                meta,
            } => {
                meta_cache.columns.insert((table_id, column_id), meta);
            }
        }
        Ok(catalog_changed)
    }
}

#[maybe_async]
impl<S: Storage> Database<S> {
    pub(crate) async fn execute_mut<A, F>(
        &mut self,
        context: &str,
        params: A,
        build: F,
    ) -> Result<(), DatabaseError>
    where
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'a, 'txn, 'bind> FnOnce(
            &mut Binder<'bind, '_, S::TransactionType<'txn>, A>,
            &mut PlanArena<'a>,
        ) -> Result<LogicalPlan, DatabaseError>,
    {
        self.execute_mut_with(context, params, build, ()).await
    }

    pub(crate) async fn execute_mut_with<A, F, C>(
        &mut self,
        context: &str,
        params: A,
        build: F,
        consume: C,
    ) -> Result<C::Output, DatabaseError>
    where
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'a, 'txn, 'bind> FnOnce(
            &mut Binder<'bind, '_, S::TransactionType<'txn>, A>,
            &mut PlanArena<'a>,
        ) -> Result<LogicalPlan, DatabaseError>,
        C: ConsumeMut<S>,
    {
        self.state.release_retained();
        // `&mut self`: nothing else can pin this generation, so it is edited in place.
        let mut edit = self.state.edit_catalog();
        let (arena, meta) = edit.parts();
        let transaction = OwnedPtr::new(self.storage.transaction_with_functions(
            self.transaction_isolation,
            false,
            Some(self.state.scala_functions_arc()),
            Some(self.state.table_functions_arc()),
        )?);

        let (plan_arena, executor) = unsafe {
            self.state
                .execute_mut(arena, &*meta, &mut *transaction.get(), params, build, false)
                .await
        }
        .map_err(|err| err.with_sql_context(context))?;

        let mut iter = TransactionIter::new(plan_arena, executor);
        let result = consume
            .consume(&mut iter)
            .await
            .map_err(|err| err.with_sql_context(context))?;
        let (mut plan_arena, apply) = iter
            .done_with_ddl_apply()
            .await
            .map_err(|err| err.with_sql_context(context))?;

        unsafe { transaction.into_inner() }
            .commit()
            .await
            .map_err(|err| err.with_sql_context(context))?;

        let mut catalog_changed = false;
        for apply_item in apply {
            catalog_changed |= apply_item
                .apply_to(arena, meta, &mut plan_arena)
                .map_err(|err| err.with_sql_context(context))?;
        }
        if catalog_changed {
            State::<S>::recycle_arena(arena, &self.state.function_registry)?;
        }
        Ok(result)
    }

    /// Publishes the committed catalog to every statement that starts from now on, without
    /// `&mut self`: rebuilds a generation from committed storage and swaps it in.
    /// Statements and transactions already running keep the generation they pinned; the
    /// replaced generation is freed once the last of them lets go.
    pub(crate) async fn publish_catalog(&self) -> Result<(), DatabaseError> {
        loop {
            // Load before reading storage: a publisher that swaps in after this load makes the
            // swap below fail, and one that swapped before it committed before this snapshot.
            let current = self.state.published.load_full();
            let transaction = self.storage.transaction_with_functions(
                self.storage.default_transaction_isolation(),
                true,
                Some(self.state.scala_functions_arc()),
                Some(self.state.table_functions_arc()),
            )?;
            let built = self
                .state
                .build_catalog(current.view(), &transaction)
                .await?;
            drop(transaction);
            let next = Arc::new(PublishedCatalog::new(
                built.arena,
                built.meta,
                self.state.next_generation(),
                &self.state.live_catalogs,
            ));
            let previous = self.state.published.compare_and_swap(&current, next);
            if Arc::ptr_eq(&*previous, &current) {
                return Ok(());
            }
            // Lost the race to another publisher: rebuild on top of its generation.
        }
    }

    pub async fn analyze(&mut self, table_name: impl AsRef<str>) -> Result<(), DatabaseError> {
        let context = "ANALYZE";
        let table_name = table_name.as_ref();
        self.execute_mut(context, &[], move |binder, arena| {
            binder.bind_analyze(table_name, arena)
        })
        .await
    }

    pub async fn load(&mut self, kind: CatalogKind) -> Result<(), DatabaseError> {
        match kind {
            CatalogKind::ScalarFunction(function) => {
                self.state.load_scalar_function(function);
                Ok(())
            }
            CatalogKind::TableFunction(function) => self.state.load_table_function(function),
            CatalogKind::Table(name) => {
                self.state.release_retained();
                // `&mut self`: nothing else can pin this generation, so edit it in place.
                let mut edit = self.state.edit_catalog();
                let (arena, meta_cache) = edit.parts();
                let transaction = self.storage.transaction_with_functions(
                    self.storage.default_transaction_isolation(),
                    true,
                    Some(self.state.scala_functions_arc()),
                    Some(self.state.table_functions_arc()),
                )?;
                let mut table_codec = TableCodec::default();
                let table = transaction
                    .load_table(&mut table_codec, arena.borrow_mut(), &name)
                    .await?
                    .ok_or(DatabaseError::TableNotFound)?;
                let mut indexes = Vec::new();
                let mut columns = Vec::new();
                for index in table.indexes() {
                    let index_id = arena.borrow().index(*index).id;
                    if let Some(meta) = transaction
                        .index_statistics(
                            &mut table_codec,
                            table.id(),
                            index_id,
                            arena.borrow_mut(),
                        )
                        .await?
                    {
                        indexes.push((index_id, meta));
                    }
                }
                for column in table.columns() {
                    let column_id = {
                        let table_arena = arena.borrow();
                        let column = table_arena.column(*column);
                        column
                            .id()
                            .ok_or_else(|| DatabaseError::invalid_column(column.name()))?
                    };
                    if let Some(meta) = transaction
                        .column_statistics(
                            &mut table_codec,
                            table.id(),
                            column_id,
                            arena.borrow_mut(),
                        )
                        .await?
                    {
                        columns.push((column_id, meta));
                    }
                }
                let table_id = table.id();
                drop(transaction);
                meta_cache.remove_table(table_id);
                meta_cache
                    .indexes
                    .extend(indexes.into_iter().map(|(id, meta)| ((table_id, id), meta)));
                meta_cache
                    .columns
                    .extend(columns.into_iter().map(|(id, meta)| ((table_id, id), meta)));
                arena.borrow_mut().insert_table(table);
                State::<S>::recycle_arena(arena, &self.state.function_registry)
            }
            CatalogKind::View(name) => {
                self.state.release_retained();
                let mut edit = self.state.edit_catalog();
                let (arena, _) = edit.parts();
                let transaction = self.storage.transaction_with_functions(
                    self.storage.default_transaction_isolation(),
                    true,
                    Some(self.state.scala_functions_arc()),
                    Some(self.state.table_functions_arc()),
                )?;
                let mut table_codec = TableCodec::default();
                let (view_id, view) = transaction
                    .load_view(
                        &mut table_codec,
                        arena,
                        &self.state.function_registry,
                        name.clone(),
                    )
                    .await?
                    .ok_or(DatabaseError::ViewNotFound)?;
                drop(transaction);
                arena.borrow_mut().insert_view(view_id, view);
                State::<S>::recycle_arena(arena, &self.state.function_registry)
            }
        }
    }

    /// Opens a new explicit transaction.
    ///
    /// Statements executed through the returned transaction share the same
    /// transactional context until [`Transaction::commit`] is called.
    /// A `read_only` transaction rejects statements that write; on LMDB it runs
    /// alongside the single writer instead of waiting for it.
    /// Creates a new transaction with statement-level atomicity.
    ///
    /// When a DML statement fails partway through, only that statement's changes
    /// are rolled back while the transaction remains open. This matches MySQL
    /// semantics and is the default for explicit transactions.
    ///
    /// For read-only transactions, statement undo is skipped as an optimization.
    pub fn new_transaction(&self, read_only: bool) -> Result<Transaction<'_, S>, DatabaseError> {
        self.new_transaction_with_undo(read_only, !read_only)
    }

    /// Creates a new transaction without statement-level undo.
    ///
    /// Use this for implicit autocommit-style patterns where the entire transaction
    /// is dropped on failure. Skips undo recording for performance.
    pub fn new_transaction_without_undo(
        &self,
        read_only: bool,
    ) -> Result<Transaction<'_, S>, DatabaseError> {
        self.new_transaction_with_undo(read_only, false)
    }

    /// Creates a new transaction with explicit undo control.
    ///
    /// `enable_undo`: Whether to enable statement-level undo for this transaction.
    /// Should be true for explicit transactions to allow statements to be rolled back
    /// individually while keeping the transaction open. Should be false for implicit
    /// autocommit transactions where the entire transaction is dropped on failure.
    pub fn new_transaction_with_undo(
        &self,
        read_only: bool,
        enable_undo: bool,
    ) -> Result<Transaction<'_, S>, DatabaseError> {
        let catalog = self.state.catalog();
        let transaction = self.storage.transaction_with_functions(
            self.transaction_isolation,
            read_only,
            Some(self.state.scala_functions_arc()),
            Some(self.state.table_functions_arc()),
        )?;

        Ok(Transaction {
            inner: transaction,
            state: &self.state,
            catalog,
            read_only,
            enable_undo,
            db_id: self.id.clone(),
        })
    }

    /// Returns storage-engine-specific metrics when supported.
    ///
    /// To enable metrics collection when the underlying storage supports it,
    /// configure the database with [`DataBaseBuilder::storage_statistics(true)`](crate::db::DataBaseBuilder::storage_statistics)
    /// during construction. If metrics collection is not enabled, this returns
    /// `None`.
    #[inline]
    pub fn storage_metrics(&self) -> Option<S::Metrics> {
        self.storage.metrics()
    }

    #[inline]
    pub fn transaction_isolation(&self) -> TransactionIsolationLevel {
        self.transaction_isolation
    }

    /// Creates a session for SQL transaction control.
    ///
    /// The session borrows the database and provides BEGIN, COMMIT, ROLLBACK,
    /// and SET autocommit control. See [`crate::db::session::Session`] for usage patterns.
    #[cfg(feature = "parser")]
    pub fn session(&self) -> crate::db::session::Session<'_, S> {
        crate::db::session::Session::new(self)
    }
}

#[maybe_async]
pub trait ConsumeMut<S: Storage> {
    type Output;

    async fn consume<'a>(
        self,
        iter: &mut TransactionIter<'a, 'a, S::TransactionType<'a>, PlanArena<'a>>,
    ) -> Result<Self::Output, DatabaseError>;
}

#[maybe_async]
impl<S: Storage> ConsumeMut<S> for () {
    type Output = ();

    async fn consume<'a>(
        self,
        _: &mut TransactionIter<'a, 'a, S::TransactionType<'a>, PlanArena<'a>>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
}

#[maybe_async]
impl<'a, S: Storage> BindSource for &'a Database<S> {
    type Iter = DatabaseIter<'a, S>;
    type Transaction = S::TransactionType<'a>;

    type Storage = S;

    async fn execute<F>(self, read_only: bool, build: F) -> Result<Self::Iter, DatabaseError>
    where
        F: for<'s> FnOnce(
                &'s State<S>,
                CatalogView<'s>,
                &Self::Transaction,
            ) -> Result<(Executor<'s>, PlanArena<'s>), DatabaseError>
            + MaybeSend,
    {
        let transaction = OwnedPtr::new(PinnedTransaction {
            transaction: self.storage.transaction_with_functions(
                self.transaction_isolation,
                read_only,
                Some(self.state.scala_functions_arc()),
                Some(self.state.table_functions_arc()),
            )?,
            catalog: self.state.catalog(),
        });
        // SAFETY: the transaction and its pinned catalog stay boxed, and so in place, until
        // the iterator is gone.
        let PinnedTransaction {
            transaction: tx,
            catalog,
        } = unsafe { &mut *transaction.get() };
        let (executor, arena) = build(&self.state, catalog.view(), tx)?;
        // Disable undo for autocommit: entire transaction is dropped on failure
        let (arena, executor) = self.state.execute(tx, executor, arena, false).await?;
        Ok(DatabaseIter {
            inner: Some(OwnedPtr::new(TransactionIter::new(arena, executor))),
            transaction: Some(transaction),
        })
    }

    #[cfg(feature = "orm")]
    fn explain<A, F>(self, params: A, build: F) -> Result<String, DatabaseError>
    where
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'bind> FnOnce(
            &mut Binder<'bind, '_, Self::Transaction, A>,
            &mut PlanArena<'_>,
        ) -> Result<LogicalPlan, DatabaseError>,
    {
        let transaction = self.storage.transaction_with_functions(
            self.transaction_isolation,
            true,
            Some(self.state.scala_functions_arc()),
            Some(self.state.table_functions_arc()),
        )?;
        let catalog = self.state.catalog();
        let (plan, mut arena) =
            self.state
                .build_plan_in(catalog.view(), params, &transaction, build)?;
        Ok(plan.explain(&mut arena, 0))
    }
}

impl<S> Database<S>
where
    S: CheckpointableStorage,
{
    /// Creates an online consistent checkpoint in `path`.
    ///
    /// The target path must not exist or must be an empty directory.
    #[inline]
    pub fn checkpoint<P: AsRef<Path>>(&self, path: P) -> Result<(), DatabaseError> {
        self.storage.create_checkpoint(path)
    }
}

/// A result row, lent to the callback of an iterator's `next_tuple`.
///
/// It reads as a [`TupleImpl`] without copying. Its strings and binaries may point into the
/// transaction's reads, so values leave the row copied out of those through [`ResultRow::take`],
/// [`ResultRow::take_values`], [`ResultRow::take_value`] or `clone`, or lent for `'txn` through
/// [`ResultRow::take_txn_value`]. `'txn` lasts as long as [`TransactionIter`] or `ProgramIter`
/// borrows its transaction, and only the callback for [`DatabaseIter`] or through
/// [`ResultIter`].
#[derive(Debug)]
pub struct ResultRow<'r, 'txn> {
    tuple: &'r mut TupleImpl,
    _txn: PhantomData<&'txn ()>,
}

impl<'r, 'txn> ResultRow<'r, 'txn> {
    pub fn new(tuple: &'r mut TupleImpl) -> Self {
        Self {
            tuple,
            _txn: PhantomData,
        }
    }

    /// Lends `row` after copying its values out of the transaction's reads, so nothing it hands
    /// out borrows the transaction, whatever `'txn` is.
    #[doc(hidden)]
    pub fn materialized(row: &'r mut ResultRow<'_, '_>) -> Self {
        row.tuple.materialize();
        Self::new(&mut *row.tuple)
    }

    /// Takes the whole row, leaving it empty.
    pub fn take(&mut self) -> TupleImpl {
        let mut tuple = mem::take(self.tuple);
        tuple.materialize();
        tuple
    }

    /// Takes every value of the row, leaving it empty.
    pub fn take_values(&mut self) -> Vec<DataValue> {
        let mut values = self.tuple.take_values();
        values.iter_mut().for_each(DataValue::materialize);
        values
    }

    /// Takes the value at `index`, leaving NULL in its place.
    pub fn take_value(&mut self, index: usize) -> Option<DataValue> {
        let mut value = mem::replace(self.tuple.get_mut(index)?, DataValue::Null);
        value.materialize();
        Some(value)
    }

    /// Takes the value at `index` without copying its strings and binaries, leaving NULL in its
    /// place.
    pub fn take_txn_value(&mut self, index: usize) -> Option<TxnValue<'txn>> {
        let value = mem::replace(self.tuple.get_mut(index)?, DataValue::Null);
        Some(TxnValue {
            value,
            _txn: PhantomData,
        })
    }
}

impl Deref for ResultRow<'_, '_> {
    type Target = TupleImpl;

    fn deref(&self) -> &TupleImpl {
        self.tuple
    }
}

/// A value taken from a [`ResultRow`] without copying. Its strings and binaries may still point
/// into the transaction's reads, which stay valid for `'txn`.
#[derive(Debug)]
pub struct TxnValue<'txn> {
    value: DataValue,
    _txn: PhantomData<&'txn ()>,
}

impl<'txn> TxnValue<'txn> {
    pub fn is_null(&self) -> bool {
        self.value.is_null()
    }

    pub fn cast(self, ty: &LogicalType) -> Result<Self, DatabaseError> {
        Ok(Self {
            value: self.value.cast(ty)?,
            _txn: PhantomData,
        })
    }

    pub fn into_str(self) -> Result<Cow<'txn, str>, DataValue> {
        match self.value {
            // SAFETY: a mapped value comes from the transaction borrowed for `'txn`, which cannot
            // end before `'txn` does.
            DataValue::Utf8 { value, .. } => Ok(unsafe { value.into_cow() }),
            value => Err(value),
        }
    }

    pub fn into_bytes(self) -> Result<Cow<'txn, [u8]>, DataValue> {
        match self.value {
            // SAFETY: as in `into_str`.
            DataValue::Binary { value, .. } => Ok(unsafe { value.into_cow() }),
            value => Err(value),
        }
    }

    pub fn into_value(self) -> DataValue {
        let mut value = self.value;
        value.materialize();
        value
    }
}

/// A result iterator that borrows its transaction for `'txn`, such as [`TransactionIter`], so its
/// rows lend values for as long.
#[maybe_async]
pub trait TxnResultIter<'txn>: ResultIter {
    /// Like [`ResultIter::next_tuple`], but the row lends values for `'txn`. The iterators' own
    /// `next_tuple` does the same; this is for code generic over them.
    async fn next_txn_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, 'txn>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError>;

    #[cfg(feature = "orm")]
    fn txn_orm<T>(self) -> TxnOrmIter<'txn, Self, T>
    where
        Self: Sized,
        T: FromTxnRow<'txn>,
    {
        TxnOrmIter::new(self)
    }
}

/// Common interface for result iterators returned by database execution APIs.
#[maybe_async]
pub trait ResultIter {
    /// Borrows the output schema for the current result set.
    fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R;

    /// Advances to the next row and runs `f` while the executor row slot is borrowed.
    async fn next_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, '_>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError>;

    /// Finishes consuming the iterator and flushes any remaining work.
    async fn done(self) -> Result<(), DatabaseError>;

    #[cfg(feature = "orm")]
    /// Converts this iterator into a typed ORM iterator.
    ///
    /// This is available when the `orm` feature is enabled and the target type
    /// implements `FromQueryRow`, which is typically generated by
    /// `#[derive(Model)]` or `#[derive(Projection)]`.
    fn orm<T>(self) -> OrmIter<Self, T>
    where
        Self: Sized,
        T: FromQueryRow,
    {
        OrmIter::new(self)
    }
}

#[maybe_async]
#[cfg(feature = "orm")]
#[sync_only]
pub trait AsyncIter: Iterator {}

#[maybe_async]
#[cfg(feature = "orm")]
#[async_only]
pub trait AsyncIter {
    type Item;

    /// Reads the next item, or `None` once the rows run out.
    async fn next(&mut self) -> Option<Self::Item>;
}

#[cfg(feature = "orm")]
/// Typed adapter over a [`ResultIter`] that yields ORM models instead of raw tuples.
pub struct OrmIter<I, T> {
    inner: I,
    _marker: PhantomData<T>,
}

#[maybe_async]
#[cfg(feature = "orm")]
impl<I, T> OrmIter<I, T>
where
    I: ResultIter,
    T: FromQueryRow,
{
    fn new(inner: I) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Borrows the schema of the underlying result set.
    pub fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R {
        self.inner.schema(f)
    }

    /// Finishes the underlying raw iterator.
    pub async fn done(self) -> Result<(), DatabaseError> {
        self.inner.done().await
    }
}

#[maybe_async]
#[cfg(feature = "orm")]
impl<I, T> AsyncIter for OrmIter<I, T>
where
    I: ResultIter + MaybeSend,
    T: FromQueryRow + MaybeSend,
{
    #[async_only]
    type Item = Result<T, DatabaseError>;

    /// Reads the next model.
    #[async_only]
    async fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next_tuple(|schema, tuple| T::from_query_row(schema, tuple))
            .await
            .transpose()
            .map(|row| row.and_then(std::convert::identity))
    }
}

#[maybe_async]
#[cfg(feature = "orm")]
#[sync_only]
impl<I, T> Iterator for OrmIter<I, T>
where
    I: ResultIter,
    T: FromQueryRow,
{
    type Item = Result<T, DatabaseError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next_tuple(|schema, tuple| T::from_query_row(schema, tuple))
            .transpose()
            .map(|row| row.and_then(std::convert::identity))
    }
}

#[cfg(feature = "orm")]
pub struct TxnOrmIter<'txn, I, T> {
    inner: I,
    _marker: PhantomData<(&'txn (), T)>,
}

#[maybe_async]
#[cfg(feature = "orm")]
impl<'txn, I, T> TxnOrmIter<'txn, I, T>
where
    I: TxnResultIter<'txn>,
    T: FromTxnRow<'txn>,
{
    fn new(inner: I) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    pub async fn done(self) -> Result<(), DatabaseError> {
        self.inner.done().await
    }
}

#[maybe_async]
#[cfg(feature = "orm")]
impl<'txn, I, T> AsyncIter for TxnOrmIter<'txn, I, T>
where
    I: TxnResultIter<'txn> + MaybeSend,
    T: FromTxnRow<'txn> + MaybeSend,
{
    #[async_only]
    type Item = Result<T, DatabaseError>;

    /// Reads the next model.
    #[async_only]
    async fn next(&mut self) -> Option<Self::Item> {
        // Boxed as `dyn Future + Send` here, where `I: TxnResultIter<'txn>` holds: inside the
        // future of `next` the compiler loses that bound when it checks `Send`.
        let row: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send + '_>> = Box::pin(
            self.inner
                .next_txn_tuple(|schema, row| T::from_txn_row(schema, row)),
        );
        row.await
            .transpose()
            .map(|row| row.and_then(std::convert::identity))
    }
}

#[maybe_async]
#[cfg(feature = "orm")]
#[sync_only]
impl<'txn, I, T> Iterator for TxnOrmIter<'txn, I, T>
where
    I: TxnResultIter<'txn>,
    T: FromTxnRow<'txn>,
{
    type Item = Result<T, DatabaseError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next_txn_tuple(|schema, row| T::from_txn_row(schema, row))
            .transpose()
            .map(|row| row.and_then(std::convert::identity))
    }
}

/// Raw result iterator returned by database execution APIs.
pub struct DatabaseIter<'a, S: Storage + 'a> {
    // Dropped before `transaction`, which it borrows; `done` takes both out.
    pub(crate) inner: Option<OwnedPtr<TransactionIter<'a, 'a, S::TransactionType<'a>>>>,
    pub(crate) transaction: Option<OwnedPtr<PinnedTransaction<'a, S>>>,
}

/// An autocommit transaction with the catalog generation its statements were bound against.
/// Boxed together in [`DatabaseIter`], so the generation lives exactly as long as the plans.
pub(crate) struct PinnedTransaction<'a, S: Storage + 'a> {
    pub(crate) transaction: S::TransactionType<'a>,
    pub(crate) catalog: CatalogRef,
}

#[maybe_async]
impl<'a, S: Storage> DatabaseIter<'a, S> {
    fn inner(&self) -> &TransactionIter<'a, 'a, S::TransactionType<'a>> {
        let inner = self.inner.as_ref().expect("iterator still running");
        // SAFETY: `inner` is owned by `self`, and shared here as `self` is.
        unsafe { &*inner.get() }
    }

    fn inner_mut(&mut self) -> &mut TransactionIter<'a, 'a, S::TransactionType<'a>> {
        let inner = self.inner.as_ref().expect("iterator still running");
        // SAFETY: `inner` is owned by `self`, which is borrowed mutably.
        unsafe { &mut *inner.get() }
    }

    #[inline]
    pub fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R {
        self.inner().schema(f)
    }

    #[inline]
    pub async fn next_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, '_>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        self.inner_mut().next_tuple(f).await
    }

    #[inline]
    pub async fn done(mut self) -> Result<(), DatabaseError> {
        if let Some(inner) = self.inner.take() {
            // SAFETY: taken out of `self`, so this is the only owner left.
            unsafe { inner.into_inner() }.done().await?;
        }
        if let Some(transaction) = self.transaction.take() {
            // SAFETY: the iterator that borrowed the transaction is gone.
            unsafe { transaction.into_inner() }
                .transaction
                .commit()
                .await?;
        }
        Ok(())
    }
}

#[maybe_async]
impl<S: Storage> ResultIter for DatabaseIter<'_, S> {
    fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R {
        DatabaseIter::schema(self, f)
    }

    async fn next_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, '_>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        DatabaseIter::next_tuple(self, f).await
    }

    async fn done(self) -> Result<(), DatabaseError> {
        DatabaseIter::done(self).await
    }
}

/// Explicit transaction handle created by [`Database::new_transaction`].
pub struct Transaction<'a, S: Storage + 'a> {
    pub(crate) inner: S::TransactionType<'a>,
    pub(crate) state: &'a State<S>,
    /// The catalog generation this transaction binds against, pinned when the transaction
    /// was created. All statements in this transaction use this generation, never the
    /// latest one, ensuring consistent schema even if other sessions publish DDL.
    pub(crate) catalog: CatalogRef,
    pub(crate) read_only: bool,
    pub(crate) enable_undo: bool,
    /// Database instance ID this transaction belongs to.
    pub(crate) db_id: Arc<()>,
}

#[maybe_async]
impl<'txn, S: Storage> Transaction<'txn, S> {
    /// Commits the current transaction.
    pub async fn commit(self) -> Result<(), DatabaseError> {
        self.inner.commit().await?;

        Ok(())
    }
}

#[maybe_async]
impl<'a, 'txn, S: Storage> BindSource for &'a mut Transaction<'txn, S> {
    type Iter = TransactionIter<'a, 'a, S::TransactionType<'txn>>;
    type Transaction = S::TransactionType<'txn>;

    type Storage = S;

    async fn execute<F>(self, _read_only: bool, build: F) -> Result<Self::Iter, DatabaseError>
    where
        F: for<'s> FnOnce(
                &'s State<S>,
                CatalogView<'s>,
                &Self::Transaction,
            ) -> Result<(Executor<'s>, PlanArena<'s>), DatabaseError>
            + MaybeSend,
    {
        // Pass the transaction's pinned catalog to the build closure, ensuring ORM
        // operations bind against the same generation throughout the transaction.
        let (executor, arena) = build(self.state, self.catalog.view(), &self.inner)?;
        if self.read_only && !executor.plan().operator.command_type().is_read_only() {
            return Err(DatabaseError::UnsupportedStmt(
                "cannot write in a read-only transaction".into(),
            ));
        }
        let (arena, executor) = self
            .state
            .execute(&mut self.inner, executor, arena, self.enable_undo)
            .await?;
        Ok(TransactionIter::new(arena, executor))
    }

    #[cfg(feature = "orm")]
    fn explain<A, F>(self, params: A, build: F) -> Result<String, DatabaseError>
    where
        A: AsRef<[(usize, LogicalType)]>,
        F: for<'bind> FnOnce(
            &mut Binder<'bind, '_, Self::Transaction, A>,
            &mut PlanArena<'_>,
        ) -> Result<LogicalPlan, DatabaseError>,
    {
        let (plan, mut arena) =
            self.state
                .build_plan_in(self.catalog.view(), params, &self.inner, build)?;
        Ok(plan.explain(&mut arena, 0))
    }
}

struct TransactionGuard<'a, 'p, T: StorageTransaction + 'p> {
    arena: ExecArena<'p, 'a, T>,
    active: bool,
}

impl<T: StorageTransaction> TransactionGuard<'_, '_, T> {
    #[inline]
    fn finish(&mut self, buffer: Option<&mut ExecBuffer>) {
        if !self.active {
            return;
        }
        self.active = false;
        self.arena.release_iters(buffer);
    }
}

#[maybe_async]
impl<'p, T: StorageTransaction + 'p> TransactionGuard<'_, 'p, T> {
    /// Pulls the next row of `executor`, finishing the execution once the rows run out
    /// or the execution fails.
    ///
    /// The row lends values for `'txn`, which the caller must not let outlive the transaction.
    #[inline]
    async fn next_tuple<'txn, A: MetaArena + 'p, R>(
        &mut self,
        executor: &mut Executor<'p>,
        plan_arena: &mut ExecMetaArena<A>,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, 'txn>) -> R,
    ) -> Result<Option<R>, DatabaseError> {
        if !self.active {
            return Ok(None);
        }
        match executor.next_tuple(&mut self.arena, plan_arena).await {
            Ok(true) => {
                let schema = SchemaView::new(&executor.schema, plan_arena);
                Ok(Some(f(
                    &schema,
                    &mut ResultRow::new(executor.buffer.result_mut()),
                )))
            }
            Ok(false) => {
                self.arena.clear_undo_log();
                self.finish(Some(&mut executor.buffer));
                Ok(None)
            }
            Err(original_err) => {
                // Release iterators before rollback to avoid &mut aliasing
                self.arena.release_iters(Some(&mut executor.buffer));

                let undo_log = self.arena.take_undo_log();
                let rollback_result = {
                    // Now safe to access transaction mutably since iters are released
                    let mut state = ExecArenaLocalState {
                        transaction: self.arena.transaction,
                        table_codec: &mut executor.buffer.table_codec,
                        bound_buffers: &mut executor.buffer.bound_buffers,
                        result: &mut executor.buffer.result,
                        plan_arena,
                        iters: &mut self.arena.iters,
                        ddl_apply: &mut self.arena.ddl_apply,
                    };
                    let (transaction, table_codec) = state.transaction_codec_mut();
                    undo_log.rollback(transaction, table_codec).await
                };

                // Mark guard as inactive (iters already recycled above)
                self.active = false;

                match rollback_result {
                    Ok(()) => Err(original_err),
                    Err(rollback_err) => Err(DatabaseError::StatementRollbackFailed {
                        statement_error: original_err.to_string(),
                        rollback_error: rollback_err.to_string(),
                    }),
                }
            }
        }
    }
}

impl<T: StorageTransaction> Drop for TransactionGuard<'_, '_, T> {
    fn drop(&mut self) {
        self.finish(None);
    }
}

/// Raw result iterator returned by transaction execution APIs.
///
/// `'a` borrows the transaction, `'p` the plan its [`Executor`] runs.
pub struct TransactionIter<'a, 'p, T: StorageTransaction + 'p, A: MetaArena + 'p = PlanArena<'p>> {
    // Drop execution before the metadata arena it borrows from, and before the
    // executor, whose plan the storage iterators in the guard may borrow.
    guard: TransactionGuard<'a, 'p, T>,
    executor: Executor<'p>,
    plan_arena: ExecMetaArena<A>,
}

#[maybe_async]
impl<'a, 'p, T: StorageTransaction + 'p, A: MetaArena + 'p> TransactionIter<'a, 'p, T, A> {
    pub(crate) fn new(
        plan_arena: ExecMetaArena<A>,
        (executor, arena): (Executor<'p>, ExecArena<'p, 'a, T>),
    ) -> Self {
        Self {
            guard: TransactionGuard {
                arena,
                active: true,
            },
            executor,
            plan_arena,
        }
    }

    #[inline]
    pub fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R {
        f(&SchemaView::new(&self.executor.schema, &self.plan_arena))
    }

    /// Advances to the next row and runs `f` on it. The iterator borrows its transaction for
    /// `'a`, so the row lends values for as long (see [`ResultRow::take_txn_value`]).
    #[inline]
    pub async fn next_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, 'a>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        self.guard
            .next_tuple(&mut self.executor, &mut self.plan_arena, f)
            .await
    }

    #[inline]
    pub async fn done(mut self) -> Result<(), DatabaseError> {
        while self.next_tuple(|_, _| ()).await?.is_some() {}
        Ok(())
    }
}

#[maybe_async]
impl<'a, T: StorageTransaction + 'a> TransactionIter<'a, 'a, T, PlanArena<'a>> {
    async fn done_with_ddl_apply(
        mut self,
    ) -> Result<(PlanArena<'a>, Vec<DDLApply>), DatabaseError> {
        while self.next_tuple(|_, _| ()).await?.is_some() {}
        let Self {
            mut guard,
            plan_arena,
            ..
        } = self;
        let ddl_apply = guard.arena.take_ddl_apply();
        drop(guard);
        Ok((plan_arena.into_parent(), ddl_apply))
    }
}

#[maybe_async]
impl<'p, T: StorageTransaction + 'p, A: MetaArena + 'p> ResultIter
    for TransactionIter<'_, 'p, T, A>
{
    fn schema<R>(&self, f: impl FnOnce(&SchemaView<'_, '_>) -> R) -> R {
        TransactionIter::schema(self, f)
    }

    async fn next_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, '_>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        TransactionIter::next_tuple(self, f).await
    }

    async fn done(self) -> Result<(), DatabaseError> {
        TransactionIter::done(self).await
    }
}

#[maybe_async]
impl<'a, 'p, T: StorageTransaction + 'p, A: MetaArena + 'p> TxnResultIter<'a>
    for TransactionIter<'a, 'p, T, A>
{
    async fn next_txn_tuple<R: MaybeSend>(
        &mut self,
        f: impl FnOnce(&SchemaView<'_, '_>, &mut ResultRow<'_, 'a>) -> R + MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        TransactionIter::next_tuple(self, f).await
    }
}

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
pub(crate) mod test {
    use crate::binder::{Binder, BinderContext};
    use crate::catalog::{ColumnCatalog, ColumnDesc};
    use crate::db::{optimizer_pipeline, CatalogKind, DataBaseBuilder, DatabaseError, ResultIter};
    use crate::execution::dql::projection::Projection;
    use crate::execution::execute_input;
    use crate::expression::{AliasType, BinaryOperator, ScalarExpression};
    use crate::optimizer::core::statistics_meta::StatisticMetaLoader;
    use crate::planner::operator::join::{JoinCondition, JoinOperator, JoinType};
    use crate::planner::operator::project::ProjectOperator;
    use crate::planner::operator::table_scan::TableScanOperator;
    use crate::planner::operator::values::ValuesOperator;
    use crate::planner::operator::Operator;
    use crate::planner::{Childrens, ExprRef, LogicalPlan, PlanArena, TableArenaCell};
    #[cfg(feature = "rocksdb")]
    use crate::storage::TransactionIsolationLevel;
    use crate::storage::{
        table_codec::TableCodec, NameMapping, StatisticsMetaCache, Storage, StorageTransaction,
        TableCache,
    };
    use crate::types::tuple::{Tuple, TupleImpl};
    use crate::types::value::DataValue;
    use crate::types::LogicalType;
    use chrono::{Datelike, Local};
    #[cfg(feature = "rocksdb")]
    use std::io::ErrorKind;
    #[cfg(feature = "unsafe_txdb_checkpoint")]
    use std::sync::atomic::AtomicUsize;
    #[cfg(feature = "unsafe_txdb_checkpoint")]
    use std::sync::atomic::Ordering;
    #[cfg(feature = "unsafe_txdb_checkpoint")]
    use std::sync::Arc;
    #[cfg(feature = "unsafe_txdb_checkpoint")]
    use std::thread;
    #[cfg(feature = "unsafe_txdb_checkpoint")]
    use std::time::Duration;
    use tempfile::TempDir;

    fn table_id(name: &str) -> crate::catalog::TableId {
        name.bytes()
            .fold(0, |id, byte| id << 8 | crate::catalog::TableId::from(byte))
    }

    fn literal_input(
        name: &str,
        values: &[i32],
        position: usize,
        arena: &mut PlanArena,
    ) -> Result<(LogicalPlan, ExprRef), DatabaseError> {
        let mut column = ColumnCatalog::new(
            name.into(),
            false,
            ColumnDesc::new(LogicalType::Integer, None, false, None)?,
        );
        column.set_table(table_id(name), 0);
        let column = arena.alloc_column(column);
        let key = arena.alloc_expression(ScalarExpression::column_expr(column, position));
        let rows = values
            .iter()
            .map(|value| {
                arena.alloc_expression(ScalarExpression::Constant(DataValue::Int32(*value)))
            })
            .collect();
        Ok((
            LogicalPlan::new(
                Operator::Values(ValuesOperator {
                    rows,
                    row_count: values.len(),
                    schema_ref: vec![column],
                }),
                Childrens::None,
            ),
            key,
        ))
    }

    #[test]
    async fn late_project_collapse_preserves_oriented_columns_and_aliases(
    ) -> Result<(), DatabaseError> {
        for alias in [false, true] {
            let tables = TableArenaCell::default();
            let mut arena = PlanArena::new(&tables);
            let (left, large) = literal_input("large", &[1, 2, 3, 4], 0, &mut arena)?;
            let (right, small) = literal_input("small", &[99], 1, &mut arena)?;
            let join =
                JoinOperator::build(left, right, JoinCondition::None, JoinType::Cross, false);
            let output = if alias {
                arena.alloc_expression(ScalarExpression::Alias {
                    expr: large,
                    alias: AliasType::Name("wanted".into()),
                })
            } else {
                large
            };
            // Repeating the selected column must not lose an output slot.
            let mut plan = LogicalPlan::new(
                Operator::Project(ProjectOperator::new(vec![output, output, small])),
                Childrens::Only(Box::new(join)),
            );
            let schema = plan.output_schema(&mut arena).clone();
            let cache = StatisticsMetaCache::default();
            let loader = StatisticMetaLoader::new(&cache);
            let mut plan = optimizer_pipeline()
                .instantiate(plan)
                .find_best(Some(&loader), &mut arena)?;
            plan.populate_output_schema_recursive(&mut arena);
            assert_eq!(plan.output_schema(&mut arena), &schema);
            let Childrens::Only(join) = plan.childrens.as_ref() else {
                unreachable!()
            };
            assert!(
                matches!(join.operator, Operator::Join(_)),
                "restore Project should be collapsed"
            );
            let Childrens::Twins { left, right } = join.childrens.as_ref() else {
                unreachable!()
            };
            assert_eq!(left.estimated_rows(), Some(1));
            assert_eq!(right.estimated_rows(), Some(4));
            let Operator::Project(project) = &plan.operator else {
                unreachable!()
            };
            let Childrens::Only(input) = plan.childrens.as_ref() else {
                unreachable!()
            };
            let storage = crate::storage::new_test_storage().await;
            let transaction = storage.transaction()?;
            let table_cache = TableCache::default();
            let context = &table_cache;
            let mut executor =
                execute_input::<_, Projection>((project, input), context, arena, &transaction);
            for value in [1, 2, 3, 4] {
                assert_eq!(
                    executor.next_tuple().await?.cloned().unwrap(),
                    vec![
                        DataValue::Int32(value),
                        DataValue::Int32(value),
                        DataValue::Int32(99)
                    ]
                );
            }
            assert!(executor.next_tuple().await?.is_none());
        }
        Ok(())
    }

    #[test]
    async fn late_project_collapse_inlines_computed_output() -> Result<(), DatabaseError> {
        let tables = TableArenaCell::default();
        let mut arena = PlanArena::new(&tables);
        let (left, large) = literal_input("large", &[1, 2, 3, 4], 0, &mut arena)?;
        let (right, small) = literal_input("small", &[99], 1, &mut arena)?;
        let join = JoinOperator::build(left, right, JoinCondition::None, JoinType::Cross, false);
        let ten = arena.alloc_expression(ScalarExpression::Constant(DataValue::Int32(10)));
        let computed = arena.alloc_expression(ScalarExpression::Binary {
            op: BinaryOperator::Plus,
            left_expr: large,
            right_expr: ten,
            evaluator: None,
            ty: LogicalType::Integer,
        });
        let mut plan = LogicalPlan::new(
            Operator::Project(ProjectOperator::new(vec![computed, small])),
            Childrens::Only(Box::new(join)),
        );
        let schema = plan.output_schema(&mut arena).clone();
        let cache = StatisticsMetaCache::default();
        let loader = StatisticMetaLoader::new(&cache);
        let mut plan = optimizer_pipeline()
            .instantiate(plan)
            .find_best(Some(&loader), &mut arena)?;
        plan.populate_output_schema_recursive(&mut arena);
        assert_eq!(plan.output_schema(&mut arena), &schema);
        let Childrens::Only(child) = plan.childrens.as_ref() else {
            unreachable!()
        };
        assert!(
            matches!(child.operator, Operator::Join(_)),
            "computed expressions should be substituted into the output Project"
        );
        let Operator::Project(project) = &plan.operator else {
            unreachable!()
        };
        let Childrens::Only(input) = plan.childrens.as_ref() else {
            unreachable!()
        };
        let storage = crate::storage::new_test_storage().await;
        let transaction = storage.transaction()?;
        let table_cache = TableCache::default();
        let context = &table_cache;
        let mut executor =
            execute_input::<_, Projection>((project, input), context, arena, &transaction);
        for value in [11, 12, 13, 14] {
            assert_eq!(
                executor.next_tuple().await?.cloned().unwrap(),
                vec![DataValue::Int32(value), DataValue::Int32(99)]
            );
        }
        assert!(executor.next_tuple().await?.is_none());
        Ok(())
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn database_handles_are_send_sync() {
        #[cfg(feature = "async")]
        assert_send_sync::<super::Database<crate::storage::TestStorage>>();
        #[cfg(feature = "rocksdb")]
        assert_send_sync::<super::Database<crate::storage::rocksdb::RocksStorage>>();
        #[cfg(feature = "lmdb")]
        assert_send_sync::<super::Database<crate::storage::lmdb::LmdbStorage>>();
    }

    #[test]
    async fn scalar_query_view_references_survive_catalog_reload() -> Result<(), DatabaseError> {
        let storage = crate::storage::new_test_storage().await;
        {
            let mut db = DataBaseBuilder::path(".").build_with_storage(storage.clone())?;
            db.ddl("create view scalar_a as select (select 11) as v")
                .await?;
            db.ddl("create view scalar_b as select (select 22) as v")
                .await?;
            db.ddl("create view scalar_nested as select (select (select 33)) as v")
                .await?;
        }

        let mut db = DataBaseBuilder::path(".").build_with_storage(storage)?;
        // Loading in a different order must preserve the saved initialization references.
        for name in ["scalar_b", "scalar_a", "scalar_nested"] {
            db.load(CatalogKind::View(name.into())).await?;
        }
        let sql = "select a.v, b.v, c.v, (select 44) from scalar_a a \
                   cross join scalar_b b cross join scalar_nested c";
        let mut iter = db.run(sql).await?;
        assert_eq!(
            iter.next_tuple(|_, row| row.clone()).await?.unwrap(),
            vec![
                DataValue::Int32(11),
                DataValue::Int32(22),
                DataValue::Int32(33),
                DataValue::Int32(44),
            ]
        );
        assert!(iter.next_tuple(|_, _| ()).await?.is_none());
        iter.done().await?;

        // New definitions must not reuse references registered by the loaded views.
        db.ddl("create view scalar_new as select (select 55) as v")
            .await?;
        let mut iter = db
            .run("select a.v, b.v from scalar_a a cross join scalar_new b")
            .await?;
        assert_eq!(
            iter.next_tuple(|_, row| row.clone()).await?.unwrap(),
            vec![DataValue::Int32(11), DataValue::Int32(55)]
        );
        iter.done().await?;
        Ok(())
    }

    pub(crate) async fn build_table<T: StorageTransaction>(
        names: &mut NameMapping,
        table_cache: &mut TableCache,
        transaction: &mut T,
        plan_arena: &mut PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        let columns = vec![
            ColumnCatalog::new(
                "c1".to_string(),
                false,
                ColumnDesc::new(LogicalType::Integer, Some(0), false, None).unwrap(),
            ),
            ColumnCatalog::new(
                "c2".to_string(),
                false,
                ColumnDesc::new(LogicalType::Boolean, None, false, None).unwrap(),
            ),
            ColumnCatalog::new(
                "c3".to_string(),
                false,
                ColumnDesc::new(LogicalType::Integer, None, false, None).unwrap(),
            ),
        ];
        let mut table_codec = TableCodec::default();
        if let Some(table) = transaction
            .create_table(
                &mut table_codec,
                plan_arena,
                "t1".to_string(),
                columns,
                false,
                Vec::new(),
                Vec::new(),
                std::collections::BTreeMap::new(),
            )
            .await?
        {
            let table = table.transplant_to_table_arena(plan_arena)?;
            names.insert_table(table_cache, table);
        }

        Ok(())
    }

    async fn next_tuple_owned<I: ResultIter>(
        iter: &mut I,
    ) -> Result<Option<TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone()).await
    }

    async fn next_values<I: ResultIter>(iter: &mut I) -> Result<Option<TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone()).await
    }

    async fn read_single_i32<I>(mut iter: I) -> Result<i32, DatabaseError>
    where
        I: ResultIter,
    {
        let value = match iter
            .next_tuple(|_, tuple| match (tuple.len(), tuple.get(0)) {
                (1, Some(DataValue::Int32(value))) => *value,
                _ => panic!("expected a single Int32 column, got {tuple:?}"),
            })
            .await?
        {
            Some(value) => value,
            None => panic!("expected one result row"),
        };
        iter.done().await?;
        Ok(value)
    }

    #[cfg(feature = "unsafe_txdb_checkpoint")]
    fn query_i32<S: Storage>(
        database: &crate::db::Database<S>,
        sql: &str,
    ) -> Result<i32, DatabaseError> {
        let mut iter = database.run(sql)?;
        let value = match iter.next_tuple(|_, tuple| match (tuple.len(), tuple.get(0)) {
            (1, Some(DataValue::Int32(value))) => *value,
            _ => panic!("expected a single Int32 column, got {tuple:?}"),
        })? {
            Some(value) => value,
            None => panic!("expected one result row for query: {sql}"),
        };
        iter.done()?;
        Ok(value)
    }

    #[test]
    async fn test_run_sql() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        database
            .ddl("create table t1(c1 int primary key, c2 boolean, c3 int)")
            .await?;

        let mut iter = database.run("select * from t1").await?;
        while iter
            .next_tuple(|_, tuple| {
                println!("{tuple:#?}");
            })
            .await?
            .is_some()
        {}
        iter.done().await?;
        Ok(())
    }

    #[test]
    async fn test_group_concat_order_by_separator_distinct() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        database
            .ddl("create table gc (id int primary key, g int, b varchar(10), n int)")
            .await?;
        database
            .run("insert into gc values (1, 1, 'x', 3), (2, 1, 'y', 1), (3, 1, 'z', 2), (4, 2, 'x', null), (5, 2, null, 5), (6, 2, 'x', 4)")
            .await?
            .done()
            .await?;
        for (sql, expected) in [
            ("select group_concat(b separator '|') from gc", vec!["x|y|z|x|x"]),
            ("select group_concat(b order by b desc) from gc", vec!["z,y,x,x,x"]),
            ("select group_concat(b order by n separator '') from gc", vec!["yzxxx"]),
            ("select group_concat(distinct b order by b separator ';') from gc", vec!["x;y;z"]),
            ("select group_concat(b order by 1 desc) from gc", vec!["z,y,x,x,x"]),
            (
                "select g, group_concat(b order by id desc separator '-') from gc group by g order by g",
                vec!["1|z-y-x", "2|x-x"],
            ),
            ("select group_concat(b order by n) from gc where false", vec!["NULL"]),
            ("select group_concat(id, ':', b order by id) from gc", vec!["1:x,2:y,3:z,4:x,6:x"]),
            ("select group_concat(distinct b, g order by 1 desc, 2) from gc", vec!["z1,y1,x1,x2"]),
            ("select group_concat(b, n) from gc", vec!["x3,y1,z2,x4"]),
        ] {
            let mut iter = database.run(sql).await?;
            let mut rows = Vec::new();
            while let Some(row) = iter
                .next_tuple(|_, tuple| {
                    tuple
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .await?
            {
                rows.push(row);
            }
            iter.done().await?;
            let expected = expected.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(rows, expected, "{sql}");
        }
        for sql in [
            "select group_concat(b order by 2) from gc",
            "select group_concat(b, n order by 3) from gc",
            "select group_concat(b, n) over () from gc",
            "select group_concat(b limit 1) from gc",
            "select count(b order by b) from gc",
            "select group_concat(b separator '|') over () from gc",
        ] {
            assert!(database.run(sql).await.is_err(), "{sql} should be rejected");
        }
        Ok(())
    }

    /// use [CurrentDate](crate::function::current_date::CurrentDate) on this case
    #[test]
    async fn test_udf() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        let mut iter = kite_sql.run("select current_date()").await?;

        iter.schema(|schema| {
            assert_eq!(schema.len(), 1);
            let column = schema.get(0).unwrap();
            assert_eq!(column.name(), "current_date()");
            assert_eq!(column.datatype(), &LogicalType::Date);
        });
        assert_eq!(
            next_tuple_owned(&mut iter).await?.unwrap(),
            Tuple::new(
                None,
                vec![DataValue::Date32(Local::now().num_days_from_ce())]
            )
        );
        assert!(iter.next_tuple(|_, _| ()).await?.is_none());

        Ok(())
    }

    #[test]
    async fn test_nullif_and_ifnull_output_names() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        let mut iter = kite_sql
            .run("select nullif(1, 1), nullif(1, 2), ifnull(null, 3)")
            .await?;

        iter.schema(|schema| {
            let names: Vec<_> = schema.iter().map(|column| column.name()).collect();
            assert_eq!(names, ["nullif(1, 1)", "nullif(1, 2)", "ifnull(NULL, 3)"]);
        });
        assert_eq!(
            next_tuple_owned(&mut iter).await?.unwrap(),
            Tuple::new(
                None,
                vec![DataValue::Null, DataValue::Int32(1), DataValue::Int32(3)]
            )
        );
        assert!(iter.next_tuple(|_, _| ()).await?.is_none());

        Ok(())
    }

    /// use [Numbers](crate::function::numbers::Numbers) on this case
    #[test]
    async fn test_udtf() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        let mut iter = kite_sql.run(
            "SELECT * FROM (select * from table(numbers(10)) a ORDER BY number LIMIT 5) OFFSET 3",
        ).await?;

        iter.schema(|schema| {
            assert_eq!(schema.len(), 1);
            let column = schema.get(0).unwrap();
            assert_eq!(column.name(), "number");
            assert_eq!(column.datatype(), &LogicalType::Integer);
        });
        assert_eq!(
            next_tuple_owned(&mut iter).await?.unwrap(),
            Tuple::new(None, vec![DataValue::Int32(3)])
        );
        assert_eq!(
            next_tuple_owned(&mut iter).await?.unwrap(),
            Tuple::new(None, vec![DataValue::Int32(4)])
        );
        Ok(())
    }

    #[test]
    async fn test_join_on_alias_right_key_is_localized() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("CREATE TABLE onecolumn (id INT PRIMARY KEY, x INT NULL)")
            .await?;
        kite_sql
            .ddl("CREATE TABLE empty (e_id INT PRIMARY KEY, x INT)")
            .await?;

        let stmt = kite_sql.state.parse_statement(
            "SELECT * FROM onecolumn AS a(aid, x) JOIN empty AS b(bid, y) ON a.x = b.y",
        )?;
        let transaction = kite_sql.storage.transaction()?;
        let catalog_ref = kite_sql.state.catalog();
        let catalog = catalog_ref.view();
        let arena_borrow = catalog.arena.borrow();
        let mut binder = Binder::new(
            BinderContext::new(
                &arena_borrow.names,
                &arena_borrow.tables,
                &arena_borrow.views,
                &transaction,
                kite_sql.state.function_registry(),
                #[cfg(feature = "parser")]
                kite_sql.state.dialect(),
            ),
            &[],
            None,
        );
        let mut source_plan_arena = PlanArena::new(catalog.arena);
        let source_plan = binder.bind(&stmt, &mut source_plan_arena)?;
        let (best_plan, best_plan_arena) =
            kite_sql
                .state
                .build_plan_in(catalog, [], &transaction, |binder, arena| {
                    binder.bind(&stmt, arena)
                })?;

        let join_plan = match source_plan.operator {
            Operator::Project(_) => source_plan.childrens.pop_only(),
            Operator::Join(_) => source_plan,
            _ => unreachable!("expected a join plan"),
        };
        let Operator::Join(join_op) = join_plan.operator else {
            unreachable!("expected join operator");
        };
        let JoinCondition::On { on, filter } = join_op.on else {
            unreachable!("expected join condition");
        };
        assert!(filter.is_none());
        assert_eq!(on.len(), 1);
        let ScalarExpression::ColumnRef {
            position: left_position,
            ..
        } = source_plan_arena.expression(on[0].0.unpack_alias(&source_plan_arena))
        else {
            unreachable!("expected left join key column ref");
        };
        let ScalarExpression::ColumnRef {
            position: right_position,
            ..
        } = source_plan_arena.expression(on[0].1.unpack_alias(&source_plan_arena))
        else {
            unreachable!("expected right join key column ref");
        };
        assert_eq!(*left_position, 1);
        assert_eq!(*right_position, 1);

        let join_plan = match best_plan.operator {
            Operator::Project(_) => best_plan.childrens.pop_only(),
            Operator::Join(_) => best_plan,
            _ => unreachable!("expected a join plan"),
        };
        let Operator::Join(join_op) = join_plan.operator else {
            unreachable!("expected join operator");
        };
        let JoinCondition::On { on, .. } = join_op.on else {
            unreachable!("expected join condition");
        };
        let ScalarExpression::ColumnRef {
            position: right_position,
            ..
        } = best_plan_arena.expression(on[0].1.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected right join key column ref");
        };
        assert_eq!(*right_position, 1);

        Ok(())
    }

    #[test]
    async fn test_join_on_with_right_filter_keeps_localized_key() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("CREATE TABLE onecolumn (id INT PRIMARY KEY, x INT NULL)")
            .await?;
        kite_sql
            .ddl("CREATE TABLE twocolumn (t_id INT PRIMARY KEY, x INT NULL, y INT NULL)")
            .await?;

        let stmt = kite_sql.state.parse_statement(
            "SELECT o.x, t.y FROM onecolumn o INNER JOIN twocolumn t ON (o.x=t.x AND t.y=53)",
        )?;
        let transaction = kite_sql.storage.transaction()?;
        let catalog_ref = kite_sql.state.catalog();
        let catalog = catalog_ref.view();
        let arena_borrow = catalog.arena.borrow();
        let mut binder = Binder::new(
            BinderContext::new(
                &arena_borrow.names,
                &arena_borrow.tables,
                &arena_borrow.views,
                &transaction,
                kite_sql.state.function_registry(),
                #[cfg(feature = "parser")]
                kite_sql.state.dialect(),
            ),
            &[],
            None,
        );
        let mut source_plan_arena = PlanArena::new(catalog.arena);
        let source_plan = binder.bind(&stmt, &mut source_plan_arena)?;
        let (best_plan, best_plan_arena) =
            kite_sql
                .state
                .build_plan_in(catalog, [], &transaction, |binder, arena| {
                    binder.bind(&stmt, arena)
                })?;

        let join_plan = match source_plan.operator {
            Operator::Project(_) => source_plan.childrens.pop_only(),
            Operator::Join(_) => source_plan,
            _ => unreachable!("expected a join plan"),
        };
        let Operator::Join(join_op) = join_plan.operator else {
            unreachable!("expected join operator");
        };
        let JoinCondition::On { on, filter } = join_op.on else {
            unreachable!("expected join condition");
        };
        assert_eq!(on.len(), 1);
        let ScalarExpression::ColumnRef {
            position: left_position,
            ..
        } = source_plan_arena.expression(on[0].0.unpack_alias(&source_plan_arena))
        else {
            unreachable!("expected left join key column ref");
        };
        let ScalarExpression::ColumnRef {
            position: right_position,
            ..
        } = source_plan_arena.expression(on[0].1.unpack_alias(&source_plan_arena))
        else {
            unreachable!("expected right join key column ref");
        };
        assert_eq!(*left_position, 1);
        assert_eq!(*right_position, 1);
        let Some(filter) = filter else {
            unreachable!("expected join filter");
        };
        let mut referenced_columns = Vec::new();
        filter.all_referenced_columns(&source_plan_arena, |_, column, _| {
            referenced_columns.push(*column);
            true
        })?;
        assert_eq!(referenced_columns.len(), 1);
        assert_eq!(source_plan_arena.column(referenced_columns[0]).name(), "y");

        let join_plan = match best_plan.operator {
            Operator::Project(_) => best_plan.childrens.pop_only(),
            Operator::Join(_) => best_plan,
            _ => unreachable!("expected a join plan"),
        };
        let Operator::Join(join_op) = join_plan.operator else {
            unreachable!("expected join operator");
        };
        let JoinCondition::On { on, filter } = join_op.on else {
            unreachable!("expected join condition");
        };
        assert_eq!(on.len(), 1);
        assert!(filter.is_none());
        let ScalarExpression::ColumnRef {
            position: left_position,
            ..
        } = best_plan_arena.expression(on[0].0.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected left join key column ref");
        };
        let ScalarExpression::ColumnRef {
            position: right_position,
            ..
        } = best_plan_arena.expression(on[0].1.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected right join key column ref");
        };
        assert_eq!(*left_position, 0);
        assert_eq!(*right_position, 0);

        Ok(())
    }

    #[test]
    async fn test_join_on_with_right_filter_keeps_localized_key_with_data(
    ) -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("CREATE TABLE onecolumn (id INT PRIMARY KEY, x INT NULL)")
            .await?;
        kite_sql
            .ddl("CREATE TABLE twocolumn (t_id INT PRIMARY KEY, x INT NULL, y INT NULL)")
            .await?;
        kite_sql
            .run("INSERT INTO onecolumn(id, x) VALUES (0, 44), (1, NULL), (2, 42)")
            .await?
            .done()
            .await?;
        kite_sql
            .run(
                "INSERT INTO twocolumn(t_id, x, y) VALUES (0,44,51), (1,NULL,52), (2,42,53), (3,45,45)",
            ).await?
            .done().await?;

        let stmt = kite_sql.state.parse_statement(
            "SELECT o.x, t.y FROM onecolumn o INNER JOIN twocolumn t ON (o.x=t.x AND t.y=53)",
        )?;
        let transaction = kite_sql.storage.transaction()?;
        let catalog_ref = kite_sql.state.catalog();
        let (best_plan, best_plan_arena) = kite_sql.state.build_plan_in(
            catalog_ref.view(),
            [],
            &transaction,
            |binder, arena| binder.bind(&stmt, arena),
        )?;
        let join_plan = match best_plan.operator {
            Operator::Project(_) => best_plan.childrens.pop_only(),
            Operator::Join(_) => best_plan,
            _ => unreachable!("expected a join plan"),
        };
        let Operator::Join(join_op) = join_plan.operator else {
            unreachable!("expected join operator");
        };
        let JoinCondition::On { on, filter } = join_op.on else {
            unreachable!("expected join condition");
        };
        assert_eq!(on.len(), 1);
        assert!(filter.is_none());
        let ScalarExpression::ColumnRef {
            position: left_position,
            ..
        } = best_plan_arena.expression(on[0].0.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected left join key column ref");
        };
        let ScalarExpression::ColumnRef {
            position: right_position,
            ..
        } = best_plan_arena.expression(on[0].1.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected right join key column ref");
        };
        assert_eq!(*left_position, 0);
        assert_eq!(*right_position, 0);
        let (_, mut right_child) = join_plan.childrens.pop_twins();
        if matches!(right_child.operator, Operator::Project(_)) {
            right_child = right_child.childrens.pop_only();
        }
        let Operator::TableScan(TableScanOperator {
            predicate: Some(predicate),
            ..
        }) = right_child.operator
        else {
            unreachable!("expected pushed-down filter on right child");
        };
        let ScalarExpression::Binary {
            left_expr,
            right_expr,
            ..
        } = best_plan_arena.expression(predicate)
        else {
            unreachable!("expected binary filter predicate");
        };
        let ScalarExpression::ColumnRef {
            position: filter_position,
            ..
        } = best_plan_arena.expression(left_expr.unpack_alias(&best_plan_arena))
        else {
            unreachable!("expected filter column ref");
        };
        assert_eq!(*filter_position, 1);
        assert!(matches!(
            best_plan_arena.expression(*right_expr),
            ScalarExpression::Constant(DataValue::Int32(53))
        ));

        Ok(())
    }

    #[test]
    async fn test_prepare_statment() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .histogram_buckets(2)
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("create table t1 (a int primary key, b int)")
            .await?;
        kite_sql
            .run("insert into t1 values(0, 0)")
            .await?
            .done()
            .await?;
        kite_sql
            .run("insert into t1 values(1, 1)")
            .await?
            .done()
            .await?;
        kite_sql
            .run("insert into t1 values(2, 2)")
            .await?
            .done()
            .await?;
        kite_sql.analyze("t1").await?;
        let mut tx = kite_sql.new_transaction(false)?;

        // Filter
        {
            let mut statement = kite_sql.program_sql(
                "explain select * from t1 where b > $1",
                &[(1, LogicalType::Integer)],
            )?;

            let mut iter = statement
                .execute(&mut tx, &[(1, DataValue::Int32(0).into())])
                .await?;

            let row = next_tuple_owned(&mut iter).await?.unwrap();
            let plan = row[0].utf8().unwrap();
            assert_eq!(
                plan,
                "Projection [t1.a, t1.b] [Project => (Sort Option: Follow)] TableScan t1 -> [t1.a, t1.b], Filter: (t1.b > 0) [SeqScan => (Sort Option: None)]"
            );
        }
        // Aggregate
        {
            let mut statement = kite_sql.program_sql(
                "explain select a + $1, max(b + $2) from t1 where b > $3 group by a + $1",
                &[
                    (1, LogicalType::Integer),
                    (2, LogicalType::Integer),
                    (3, LogicalType::Integer),
                ],
            )?;

            let mut iter = statement
                .execute(
                    &mut tx,
                    &[
                        (1, DataValue::Int32(0).into()),
                        (2, DataValue::Int32(0).into()),
                        (3, DataValue::Int32(1).into()),
                        (4, DataValue::Int32(0).into()),
                    ],
                )
                .await?;
            let row = next_tuple_owned(&mut iter).await?.unwrap();
            let plan = row[0].utf8().unwrap();
            assert_eq!(
                plan,
                "Projection [(t1.a + $1), Max((t1.b + $2))] [Project => (Sort Option: Follow)] Aggregate [Max((t1.b + 0))] -> Group By [(t1.a + 0)] [HashAggregate => (Sort Option: None)] TableScan t1 -> [t1.a, t1.b], Filter: (t1.b > 1) [SeqScan => (Sort Option: None)]"
            );
        }
        {
            let mut statement = kite_sql.program_sql("explain select *, $1 from (select * from t1 where b > $2) as l left join (select * from t1 where a > $3) on l.a > $4", &[(1, LogicalType::Integer), (2, LogicalType::Integer), (3, LogicalType::Integer), (4, LogicalType::Integer)])?;

            let mut iter = statement
                .execute(
                    &mut tx,
                    &[
                        (1, DataValue::Int32(9).into()),
                        (2, DataValue::Int32(0).into()),
                        (3, DataValue::Int32(1).into()),
                        (4, DataValue::Int32(0).into()),
                    ],
                )
                .await?;
            let row = next_tuple_owned(&mut iter).await?.unwrap();
            let plan = row[0].utf8().unwrap();
            assert_eq!(
                plan,
                "Projection [#0.a, #0.b, t1.a, t1.b, 9] [Project => (Sort Option: Follow)] LeftOuter Join Where (#0.a > 0) [NestLoopJoin => (Sort Option: None)] Projection [(t1.a) as (#0.a), (t1.b) as (#0.b)] [Project => (Sort Option: Follow)] TableScan t1 -> [t1.a, t1.b], Filter: (t1.b > 0) [SeqScan => (Sort Option: None)] Projection [t1.a, t1.b] [Project => (Sort Option: Follow)] TableScan t1 -> [t1.a, t1.b] [IndexScan By pk_index => ($3, +inf) => (Sort Option: OrderBy: (t1.a Asc Nulls Last) ignore_prefix_len: 0)]"
            );
        }

        Ok(())
    }

    #[test]
    async fn test_run_multi_statement() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("create table t_multi (a int primary key, b int)")
            .await?;

        let mut iter = kite_sql.run(
            "insert into t_multi values(0, 0); insert into t_multi values(1, 1); select * from t_multi order by a",
        ).await?;
        assert_eq!(
            next_values(&mut iter).await?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter).await?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(1)]
        );
        assert!(iter.next_tuple(|_, _| ()).await?.is_none());
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_run_multi_statement_disallow_ddl() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        let err = match kite_sql
            .run("create table t_multi_ddl (a int primary key); select 1")
            .await
        {
            Ok(_) => panic!("multi-statement execution with DDL should be rejected"),
            Err(err) => err,
        };
        match err {
            DatabaseError::UnsupportedStmt(msg) => {
                assert!(msg.contains("DDL and ANALYZE"));
            }
            other => panic!("unexpected error type: {other:?}"),
        }

        Ok(())
    }

    #[test]
    async fn test_bind_error_with_span() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("create table t_bind_span(id int primary key)")
            .await?;

        let err = match kite_sql
            .run("select id, missing_col from t_bind_span")
            .await
        {
            Ok(_) => panic!("expected bind error"),
            Err(err) => err,
        };
        println!("{err}");

        match err {
            DatabaseError::ColumnNotFound { span, .. }
            | DatabaseError::InvalidColumn { span, .. } => {
                let span = span.expect("bind error should include span");
                assert_eq!(span.line, 1);
                assert!(span.start >= 12);
                assert!(span.end > span.start);
                assert!(span
                    .highlight
                    .as_deref()
                    .is_some_and(|h| h.contains("missing_col")));
            }
            other => panic!("unexpected error type: {other:?}"),
        }

        Ok(())
    }

    #[test]
    async fn test_bind_function_error_with_span() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("create table t_bind_fn_span(id int primary key)")
            .await?;

        let err = match kite_sql
            .run("select missing_fn(id) from t_bind_fn_span")
            .await
        {
            Ok(_) => panic!("expected function bind error"),
            Err(err) => err,
        };
        println!("{err}");

        match err {
            DatabaseError::FunctionNotFound { span, .. } => {
                let span = span.expect("function bind error should include span");
                assert_eq!(span.line, 1);
                assert!(span.start >= 8);
                assert!(span.end > span.start);
                assert!(span
                    .highlight
                    .as_deref()
                    .is_some_and(|h| h.contains("missing_fn(id)")));
            }
            other => panic!("unexpected error type: {other:?}"),
        }

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_transaction_sql() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path()).build_rocksdb()?;

        kite_sql.ddl("create table t1 (a int primary key, b int)")?;

        let mut tx_1 = kite_sql.new_transaction(false)?;
        let mut tx_2 = kite_sql.new_transaction(false)?;

        tx_1.run("insert into t1 values(0, 0)")?.done()?;
        tx_1.run("insert into t1 values(1, 1)")?.done()?;

        assert!(tx_2.run("insert into t1 values(0, 0)")?.done().is_err());
        tx_2.run("insert into t1 values(3, 3)")?.done()?;

        let mut iter_1 = tx_1.run("select * from t1")?;
        let mut iter_2 = tx_2.run("select * from t1")?;

        assert_eq!(
            next_values(&mut iter_1)?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter_1)?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(1)]
        );

        assert_eq!(
            next_values(&mut iter_2)?.unwrap(),
            vec![DataValue::Int32(3), DataValue::Int32(3)]
        );
        drop(iter_1);
        drop(iter_2);

        tx_1.commit()?;
        tx_2.commit()?;

        let mut tx_3 = kite_sql.new_transaction(false)?;
        let res = tx_3.run("create table t2 (a int primary key, b int)");
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    async fn test_transaction_run_multi_statement() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;

        kite_sql
            .ddl("create table t_multi_tx (a int primary key, b int)")
            .await?;

        let mut tx = kite_sql.new_transaction(false)?;
        let mut iter = tx.run(
            "insert into t_multi_tx values(0, 0); insert into t_multi_tx values(1, 1); select * from t_multi_tx order by a",
        ).await?;
        assert_eq!(
            next_values(&mut iter).await?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter).await?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(1)]
        );
        assert!(iter.next_tuple(|_, _| ()).await?.is_none());
        iter.done().await?;
        tx.commit().await?;

        let mut check_iter = kite_sql.run("select count(*) from t_multi_tx").await?;
        assert_eq!(
            next_values(&mut check_iter).await?.unwrap(),
            vec![DataValue::Int32(2)]
        );
        check_iter.done().await?;

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_autocommit_read_drops_iterator_before_transaction() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path()).build_optimistic()?;

        kite_sql.ddl("create table t_iter_drop (a int primary key, b int)")?;

        let mut tx = kite_sql.new_transaction(false)?;
        tx.run("insert into t_iter_drop values (0, 0), (1, 1)")?
            .done()?;

        let mut iter = kite_sql.run("select * from t_iter_drop")?;
        assert!(iter.next_tuple(|_, _| ())?.is_none());

        tx.commit()?;

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_exhausted_database_iter_releases_mdl_guard() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path()).build_optimistic()?;

        kite_sql.ddl("create table t_iter_guard (a int primary key, b int)")?;
        kite_sql
            .run("insert into t_iter_guard values (0, 0), (1, 1)")?
            .done()?;

        let mut iter = kite_sql.run("select * from t_iter_guard order by a")?;
        assert_eq!(
            next_values(&mut iter)?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter)?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(1)]
        );
        assert!(iter.next_tuple(|_, _| ())?.is_none());
        iter.done()?;

        kite_sql.ddl("drop table t_iter_guard")?;

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_rocksdb_defaults_to_repeatable_read() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let kite_sql = DataBaseBuilder::path(temp_dir.path()).build_rocksdb()?;

        assert_eq!(
            kite_sql.transaction_isolation(),
            TransactionIsolationLevel::RepeatableRead
        );

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_repeatable_read_keeps_transaction_snapshot() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .transaction_isolation(TransactionIsolationLevel::RepeatableRead)
            .build_rocksdb()?;

        kite_sql.ddl("create table t_rr (a int primary key, b int)")?;
        kite_sql.run("insert into t_rr values (1, 10)")?.done()?;

        let mut reader = kite_sql.new_transaction(false)?;
        let mut writer = kite_sql.new_transaction(false)?;

        assert_eq!(
            read_single_i32(reader.run("select b from t_rr where a = 1")?)?,
            10
        );

        writer.run("update t_rr set b = 20 where a = 1")?.done()?;
        writer.commit()?;

        assert_eq!(
            read_single_i32(reader.run("select b from t_rr where a = 1")?)?,
            10
        );
        reader.commit()?;

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_optimistic_transaction_sql() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path()).build_optimistic()?;

        kite_sql.ddl("create table t1 (a int primary key, b int)")?;

        let mut tx_1 = kite_sql.new_transaction(false)?;
        let mut tx_2 = kite_sql.new_transaction(false)?;

        tx_1.run("insert into t1 values(0, 0)")?.done()?;
        tx_1.run("insert into t1 values(1, 1)")?.done()?;

        tx_2.run("insert into t1 values(0, 0)")?.done()?;
        tx_2.run("insert into t1 values(3, 3)")?.done()?;

        let mut iter_1 = tx_1.run("select * from t1")?;
        let mut iter_2 = tx_2.run("select * from t1")?;

        assert_eq!(
            next_values(&mut iter_1)?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter_1)?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(1)]
        );

        assert_eq!(
            next_values(&mut iter_2)?.unwrap(),
            vec![DataValue::Int32(0), DataValue::Int32(0)]
        );
        assert_eq!(
            next_values(&mut iter_2)?.unwrap(),
            vec![DataValue::Int32(3), DataValue::Int32(3)]
        );
        drop(iter_1);
        drop(iter_2);

        tx_1.commit()?;

        assert!(tx_2.commit().is_err());

        let mut tx_3 = kite_sql.new_transaction(false)?;
        let res = tx_3.run("create table t2 (a int primary key, b int)");
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    async fn test_invalid_histogram_buckets() {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let result = DataBaseBuilder::path(temp_dir.path())
            .histogram_buckets(0)
            .build_in_memory()
            .await;

        assert!(matches!(
            result,
            Err(DatabaseError::InvalidValue(message)) if message == "histogram buckets must be >= 1"
        ));
    }

    #[test]
    async fn test_invalid_statistics_relative_error() {
        for relative_error in [0.0, 1.0, -0.5, f64::NAN] {
            let result = DataBaseBuilder::path(".")
                .statistics_relative_error(relative_error)
                .build_in_memory()
                .await;
            assert!(matches!(
                result,
                Err(DatabaseError::InvalidValue(message))
                    if message == format!("statistics relative error must be in (0, 1), got {relative_error}")
            ));
        }
    }

    #[test]
    async fn test_invalid_hll_precision() {
        for precision in [3, 27] {
            let result = DataBaseBuilder::path(".")
                .hll_precision(precision)
                .build_in_memory()
                .await;
            assert!(matches!(
                result,
                Err(DatabaseError::InvalidValue(message))
                    if message == format!("hll precision must be between 4 and 26, got {precision}")
            ));
        }
    }

    #[test]
    async fn parser_recursion_limit_applies_to_sql_frontends() -> Result<(), DatabaseError> {
        let expression = format!("{}7{}", "(".repeat(8), ")".repeat(8));
        let sql = format!("select {expression}");
        let default = DataBaseBuilder::path(".").build_in_memory().await?;
        assert_eq!(read_single_i32(default.run(&sql).await?).await?, 7);
        assert_eq!(default.prepare_all(&sql)?.len(), 1);

        let mut db = DataBaseBuilder::path(".")
            .parser_recursion_limit(256)
            .build_in_memory()
            .await?;
        assert_eq!(db.prepare_all(&sql)?.len(), 1);
        assert_eq!(read_single_i32(db.run(&sql).await?).await?, 7);
        db.ddl(format!("create view deep_view as {sql}")).await?;
        assert_eq!(
            read_single_i32(db.run("select * from deep_view").await?).await?,
            7
        );
        struct FirstRow;
        impl<S: Storage> crate::db::ConsumeMut<S> for FirstRow {
            type Output = TupleImpl;

            async fn consume<'a>(
                self,
                iter: &mut crate::db::TransactionIter<
                    'a,
                    'a,
                    S::TransactionType<'a>,
                    PlanArena<'a>,
                >,
            ) -> Result<TupleImpl, DatabaseError> {
                Ok(iter.next_tuple(|_, row| row.clone()).await?.unwrap())
            }
        }
        assert_eq!(db.run_mut(&sql, FirstRow).await?, vec![DataValue::Int32(7)]);
        let mut tx = db.new_transaction(false)?;
        let mut program = db.program_sql(&sql, &[])?;
        let mut iter = program.execute(&mut tx, []).await?;
        assert_eq!(
            iter.next_tuple(|_, row| row.clone()).await?.unwrap(),
            vec![DataValue::Int32(7)]
        );
        iter.done().await?;
        let mut iter = tx.run(&sql).await?;
        assert_eq!(
            iter.next_tuple(|_, row| row.clone()).await?.unwrap(),
            vec![DataValue::Int32(7)]
        );
        iter.done().await?;
        let mut program = tx.program_sql(&sql, &[])?;
        let mut iter = program.execute(&mut tx, []).await?;
        assert_eq!(
            iter.next_tuple(|_, row| row.clone()).await?.unwrap(),
            vec![DataValue::Int32(7)]
        );
        iter.done().await?;
        tx.commit().await?;

        let low = DataBaseBuilder::path(".")
            .parser_recursion_limit(4)
            .build_in_memory()
            .await?;
        assert!(low.run("select 1").await.is_ok());
        assert!(low
            .prepare_all(&sql)
            .unwrap_err()
            .to_string()
            .contains("recursion limit exceeded"));
        assert!(low
            .run(&sql)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("recursion limit exceeded"));
        assert!(matches!(
            DataBaseBuilder::path(".").parser_recursion_limit(0).build_in_memory().await,
            Err(DatabaseError::InvalidValue(message)) if message == "parser recursion limit must be >= 1"
        ));
        Ok(())
    }

    #[cfg(feature = "unsafe_txdb_checkpoint")]
    #[test]
    fn test_checkpoint_restores_snapshot() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let live_path = temp_dir.path().join("live");
        let checkpoint_path = temp_dir.path().join("checkpoint");
        let mut kite_sql = DataBaseBuilder::path(&live_path).build_rocksdb()?;

        kite_sql.ddl("create table t_checkpoint (id int primary key, v int)")?;
        kite_sql
            .run("insert into t_checkpoint values (1, 10), (2, 20)")?
            .done()?;

        kite_sql.checkpoint(&checkpoint_path)?;

        kite_sql
            .run("insert into t_checkpoint values (3, 30)")?
            .done()?;

        let mut snapshot = DataBaseBuilder::path(&checkpoint_path).build_rocksdb()?;
        snapshot.load(CatalogKind::Table("t_checkpoint".to_string()))?;
        assert_eq!(
            query_i32(&snapshot, "select count(*) from t_checkpoint")?,
            2
        );
        assert_eq!(
            query_i32(&kite_sql, "select count(*) from t_checkpoint")?,
            3
        );

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn optimistic_checkpoint_restores_rows_indexes_and_scalar_views() -> Result<(), DatabaseError> {
        let directory = TempDir::new()?;
        let live_path = directory.path().join("live");
        let snapshot_path = directory.path().join("snapshot");
        let mut database = DataBaseBuilder::path(&live_path).build_optimistic()?;
        database.ddl("create table checkpoint_rows(id int primary key, v int unique)")?;
        database
            .run("insert into checkpoint_rows values(1,10),(2,20)")?
            .done()?;
        database.ddl(
            "create view checkpoint_view as select (select max(v) from checkpoint_rows) as v",
        )?;
        database.checkpoint(&snapshot_path)?;
        database
            .run("update checkpoint_rows set v=30 where id=2")?
            .done()?;
        database
            .run("insert into checkpoint_rows values(3,40)")?
            .done()?;
        let mut snapshot = DataBaseBuilder::path(&snapshot_path).build_optimistic()?;
        snapshot.load(CatalogKind::Table("checkpoint_rows".into()))?;
        snapshot.load(CatalogKind::View("checkpoint_view".into()))?;
        let mut rows = snapshot.run("select id,v from checkpoint_rows order by id")?;
        assert_eq!(
            rows.next_tuple(|_, row| row.clone())?.unwrap(),
            vec![DataValue::Int32(1), DataValue::Int32(10)]
        );
        assert_eq!(
            rows.next_tuple(|_, row| row.clone())?.unwrap(),
            vec![DataValue::Int32(2), DataValue::Int32(20)]
        );
        assert!(rows.next_tuple(|_, _| ())?.is_none());
        rows.done()?;
        assert_eq!(
            read_single_i32(snapshot.run("select id from checkpoint_rows where v=20")?)?,
            2
        );
        assert_eq!(
            read_single_i32(snapshot.run("select v from checkpoint_view")?)?,
            20
        );
        assert_eq!(
            read_single_i32(database.run("select v from checkpoint_view")?)?,
            40
        );
        assert!(matches!(
            snapshot
                .run("insert into checkpoint_rows values(4,20)")?
                .done(),
            Err(DatabaseError::DuplicateUniqueValue)
        ));
        let occupied = directory.path().join("occupied");
        std::fs::write(&occupied, b"keep")?;
        assert!(
            matches!(database.checkpoint(&occupied), Err(DatabaseError::IO(ref err)) if err.kind() == ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&occupied)?, b"keep");
        drop(snapshot);
        let mut reopened = DataBaseBuilder::path(&snapshot_path).build_optimistic()?;
        reopened.load(CatalogKind::Table("checkpoint_rows".into()))?;
        assert_eq!(
            read_single_i32(reopened.run("select count(*) from checkpoint_rows")?)?,
            2
        );
        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn test_checkpoint_rejects_non_empty_target_dir() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let live_path = temp_dir.path().join("live");
        let checkpoint_path = temp_dir.path().join("checkpoint");
        let kite_sql = DataBaseBuilder::path(&live_path).build_rocksdb()?;

        std::fs::create_dir(&checkpoint_path)?;
        std::fs::write(checkpoint_path.join("stale.txt"), b"stale")?;

        let err = kite_sql
            .checkpoint(&checkpoint_path)
            .expect_err("checkpoint should reject non-empty directories");
        assert!(matches!(
            err,
            DatabaseError::IO(ref io_err) if io_err.kind() == ErrorKind::AlreadyExists
        ));

        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[cfg(not(feature = "unsafe_txdb_checkpoint"))]
    #[test]
    fn test_checkpoint_requires_unsafe_feature() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let live_path = temp_dir.path().join("live");
        let checkpoint_path = temp_dir.path().join("checkpoint");
        let mut kite_sql = DataBaseBuilder::path(&live_path).build_rocksdb()?;

        kite_sql.ddl("create table t_checkpoint_disabled (id int primary key, v int)")?;

        let err = kite_sql
            .checkpoint(&checkpoint_path)
            .expect_err("checkpoint should require the unsafe feature");
        assert!(matches!(err, DatabaseError::UnsupportedStmt(_)));

        Ok(())
    }

    #[cfg(feature = "unsafe_txdb_checkpoint")]
    #[test]
    fn test_checkpoint_during_concurrent_writes() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let live_path = temp_dir.path().join("live");
        let checkpoint_path = temp_dir.path().join("checkpoint");
        let mut kite_sql = DataBaseBuilder::path(&live_path).build_rocksdb()?;

        kite_sql.ddl("create table t_checkpoint_concurrent (id int primary key, v int)")?;
        let kite_sql = Arc::new(std::sync::Mutex::new(kite_sql));

        let inserted = Arc::new(AtomicUsize::new(0));
        let writer_db = Arc::clone(&kite_sql);
        let writer_inserted = Arc::clone(&inserted);
        let writer = thread::spawn(move || -> Result<usize, DatabaseError> {
            for i in 0..64 {
                writer_db
                    .lock()
                    .unwrap()
                    .run(format!(
                        "insert into t_checkpoint_concurrent values ({i}, {i})"
                    ))?
                    .done()?;
                writer_inserted.store(i + 1, Ordering::SeqCst);

                if i >= 8 {
                    thread::sleep(Duration::from_millis(2));
                }
            }

            Ok(64)
        });

        while inserted.load(Ordering::SeqCst) < 8 {
            thread::yield_now();
        }

        kite_sql.lock().unwrap().checkpoint(&checkpoint_path)?;

        let total = writer.join().expect("writer thread should not panic")?;
        let mut snapshot = DataBaseBuilder::path(&checkpoint_path).build_rocksdb()?;
        snapshot.load(CatalogKind::Table("t_checkpoint_concurrent".to_string()))?;
        let snapshot_count = query_i32(&snapshot, "select count(*) from t_checkpoint_concurrent")?;
        let consistent_count = query_i32(
            &snapshot,
            "select count(*) from t_checkpoint_concurrent where id = v",
        )?;

        assert!(snapshot_count >= 8);
        assert!(snapshot_count <= total as i32);
        assert_eq!(snapshot_count, consistent_count);
        assert_eq!(
            query_i32(
                &kite_sql.lock().unwrap(),
                "select count(*) from t_checkpoint_concurrent",
            )?,
            total as i32
        );

        Ok(())
    }

    #[test]
    fn test_udf_default_ddl_operations() -> Result<(), DatabaseError> {
        use crate::expression::function::scala::ScalarFunctionImpl;
        use crate::expression::function::FunctionSummary;
        use crate::planner::{ExprRef, MetaArena};
        use crate::types::tuple::Input;
        use std::sync::Arc;

        #[derive(Debug)]
        struct MyUdf {
            summary: FunctionSummary,
            return_type: LogicalType,
        }

        impl MyUdf {
            fn new() -> Self {
                Self {
                    summary: FunctionSummary {
                        name: "my_udf".into(),
                        arg_types: vec![],
                        persistence: crate::expression::function::PersistencePolicy::Durable,
                    },
                    return_type: LogicalType::Integer,
                }
            }
        }

        impl ScalarFunctionImpl for MyUdf {
            fn eval(
                &self,
                _exprs: &[ExprRef],
                _arena: &(dyn MetaArena + '_),
                _tuples: Option<&dyn Input>,
            ) -> Result<DataValue, DatabaseError> {
                Ok(DataValue::Int32(42))
            }

            fn monotonicity(&self) -> Option<crate::expression::function::scala::FuncMonotonicity> {
                None
            }

            fn return_type(&self) -> &LogicalType {
                &self.return_type
            }

            fn summary(&self) -> &FunctionSummary {
                &self.summary
            }
        }

        let my_udf = Arc::new(MyUdf::new()) as Arc<dyn ScalarFunctionImpl>;

        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut kite_sql = DataBaseBuilder::path(temp_dir.path())
            .build_with_storage(crate::storage::new_test_storage())?;

        kite_sql.load(CatalogKind::ScalarFunction(my_udf))?;

        kite_sql.ddl("CREATE TABLE t_udf (id int primary key, val int default my_udf(), v int)")?;

        kite_sql
            .run("INSERT INTO t_udf (id, v) VALUES (1, 10)")?
            .done()?;

        let mut found_val = None;
        {
            let mut iter = kite_sql.run("SELECT val FROM t_udf WHERE id = 1")?;
            while let Some(val) = iter.next_tuple(|_, row| row.get(0).cloned()).await? {
                found_val = val;
            }
            iter.done().await?;
        }
        assert_eq!(found_val, Some(DataValue::Int32(42)));

        kite_sql.ddl("CREATE INDEX ix_v ON t_udf (v)")?;
        kite_sql.ddl("DROP INDEX ix_v ON t_udf")?;
        kite_sql.ddl("ALTER TABLE t_udf ADD COLUMN val2 int default my_udf()")?;
        kite_sql.ddl("ALTER TABLE t_udf DROP COLUMN val2")?;
        kite_sql.ddl("ALTER TABLE t_udf ALTER COLUMN val SET DEFAULT my_udf()")?;
        kite_sql.ddl("ALTER TABLE t_udf ALTER COLUMN val DROP DEFAULT")?;

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn test_lmdb_function_default_persistence() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let db_path = temp_dir.path();

        {
            let mut kite_sql = DataBaseBuilder::path(db_path).build_lmdb()?;
            kite_sql.ddl("CREATE TABLE t_persist (id int primary key, d timestamp default current_timestamp, v int)")?;
            kite_sql
                .run("INSERT INTO t_persist (id, v) VALUES (1, 100)")?
                .done()?;
        }

        {
            let mut kite_sql = DataBaseBuilder::path(db_path).build_lmdb()?;
            kite_sql.load(CatalogKind::Table("t_persist".to_string()))?;
            kite_sql.ddl("CREATE INDEX ix_v ON t_persist (v)")?;
            kite_sql
                .ddl("ALTER TABLE t_persist ADD COLUMN d2 timestamp default current_timestamp")?;

            kite_sql
                .run("INSERT INTO t_persist (id, v) VALUES (2, 200)")?
                .done()?;

            let mut found_rows = Vec::new();
            {
                let mut iter = kite_sql.run("SELECT id, v, d, d2 FROM t_persist ORDER BY id")?;
                while let Some(row) = iter
                    .next_tuple(|_, row| {
                        (
                            row.get(0).cloned(),
                            row.get(1).cloned(),
                            row.get(2).cloned(),
                            row.get(3).cloned(),
                        )
                    })
                    .await?
                {
                    found_rows.push(row);
                }
                iter.done().await?;
            }

            assert_eq!(found_rows.len(), 2);

            assert_eq!(found_rows[0].0, Some(DataValue::Int32(1)));
            assert_eq!(found_rows[0].1, Some(DataValue::Int32(100)));
            assert!(
                matches!(found_rows[0].2, Some(DataValue::Time64(..))),
                "Row 0 d should be timestamp: {:?}",
                found_rows[0].2
            );

            assert_eq!(found_rows[1].0, Some(DataValue::Int32(2)));
            assert_eq!(found_rows[1].1, Some(DataValue::Int32(200)));
            assert!(
                matches!(found_rows[1].2, Some(DataValue::Time64(..))),
                "Row 1 d should be timestamp: {:?}",
                found_rows[1].2
            );
            assert!(
                matches!(found_rows[1].3, Some(DataValue::Time64(..))),
                "Row 1 d2 should be timestamp: {:?}",
                found_rows[1].3
            );
        }

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn test_function_default_cold_cache() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let db_path = temp_dir.path();

        {
            let mut kite_sql = DataBaseBuilder::path(db_path).build_lmdb()?;
            kite_sql.ddl(
                "CREATE TABLE t_cold (id int primary key, d timestamp default current_timestamp, v int)",
            )?;
            kite_sql
                .run("INSERT INTO t_cold (id, v) VALUES (1, 10)")?
                .done()?;
        }

        {
            let mut kite_sql = DataBaseBuilder::path(db_path).build_lmdb()?;
            kite_sql.load(CatalogKind::Table("t_cold".to_string()))?;

            let mut found_val = None;
            {
                let mut iter = kite_sql.run("SELECT v FROM t_cold WHERE id = 1")?;
                while let Some(val) = iter.next_tuple(|_, row| row.get(0).cloned()).await? {
                    found_val = val;
                }
                iter.done().await?;
            }
            assert_eq!(found_val, Some(DataValue::Int32(10)));
        }

        Ok(())
    }

    #[cfg(feature = "parser")]
    #[test]
    fn test_unknown_dialect_error() -> Result<(), DatabaseError> {
        let result = DataBaseBuilder::path(".").default_dialect("unknown");

        assert!(result.is_err());
        if let Err(err) = result {
            assert!(matches!(err, DatabaseError::UnsupportedStmt(_)));
        }

        Ok(())
    }

    #[cfg(feature = "parser")]
    #[test]
    fn test_mysql_dialect_requires_registration() {
        // Attempting to use MySQL dialect without registering it should give a clear error
        let result = DataBaseBuilder::path(".").default_dialect("mysql");

        assert!(result.is_err());
        if let Err(DatabaseError::UnsupportedStmt(msg)) = result {
            assert!(
                msg.contains("unknown dialect") || msg.contains("mysql"),
                "Error message should mention unknown dialect or mysql: {}",
                msg
            );
        } else {
            panic!("Expected UnsupportedStmt error for unregistered MySQL dialect");
        }
    }

    #[cfg(feature = "parser")]
    #[test]
    fn test_generic_dialect_rejects_mysql_comment_syntax() -> Result<(), DatabaseError> {
        let database_generic = DataBaseBuilder::path(".").build_in_memory()?;

        // Generic dialect should reject MySQL's `#` comment syntax
        let generic_result = database_generic
            .state
            .prepare_all("SELECT 1 # MySQL comment");
        assert!(
            generic_result.is_err(),
            "Generic dialect should reject MySQL # comments"
        );

        // Generic dialect should accept standard SQL comments
        let generic_result2 = database_generic.state.prepare_all("SELECT 1 -- comment\n");
        assert!(
            generic_result2.is_ok(),
            "Generic dialect should accept standard -- comments"
        );

        Ok(())
    }

    #[cfg(feature = "parser")]
    #[test]
    fn test_dialect_rewrite_hook() -> Result<(), DatabaseError> {
        use crate::dialect::SqlDialect;
        use sqlparser::ast::Statement;
        use std::sync::Arc;

        #[derive(Debug)]
        struct RewritingDialect {
            inner: crate::dialect::GenericSqlDialect,
        }

        impl SqlDialect for RewritingDialect {
            fn name(&self) -> &str {
                "rewriting_test"
            }

            fn parser_dialect(&self) -> &dyn sqlparser::dialect::Dialect {
                self.inner.parser_dialect()
            }

            fn rewrite(&self, stmt: &mut Statement) -> Result<(), DatabaseError> {
                if let Statement::Query(query) = stmt {
                    if let sqlparser::ast::SetExpr::Select(select) = query.body.as_mut() {
                        for proj in &mut select.projection {
                            if let sqlparser::ast::SelectItem::UnnamedExpr(
                                sqlparser::ast::Expr::Value(val),
                            ) = proj
                            {
                                if let sqlparser::ast::Value::Number(ref mut n, _) = &mut val.value
                                {
                                    if n == "42" {
                                        *n = "99".to_string();
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(())
            }
        }

        let database = DataBaseBuilder::path(".")
            .register_dialect(Arc::new(RewritingDialect {
                inner: crate::dialect::GenericSqlDialect::new(),
            }))
            .default_dialect("rewriting_test")?
            .build_in_memory()?;

        let mut result = database.run("SELECT 42")?;
        let row = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(row[0], DataValue::Int32(99));
        result.done()?;

        Ok(())
    }

    #[cfg(all(feature = "parser", feature = "lmdb"))]
    #[test]
    fn test_dialect_text_collation() -> Result<(), DatabaseError> {
        use crate::dialect::SqlDialect;
        use crate::types::collation::TextCollation;
        use std::borrow::Cow;
        use std::sync::Arc;

        fn lower(text: &str) -> Cow<'_, str> {
            if text.chars().any(char::is_uppercase) {
                Cow::Owned(text.to_lowercase())
            } else {
                Cow::Borrowed(text)
            }
        }

        #[derive(Debug)]
        struct CaseFoldingDialect {
            inner: crate::dialect::GenericSqlDialect,
        }

        impl SqlDialect for CaseFoldingDialect {
            fn name(&self) -> &str {
                "case_folding_test"
            }

            fn parser_dialect(&self) -> &dyn sqlparser::dialect::Dialect {
                self.inner.parser_dialect()
            }

            fn text_collation(&self) -> Option<TextCollation> {
                Some(TextCollation::new("lower", lower))
            }

            fn is_binary_collation(&self, name: &str) -> bool {
                name.eq_ignore_ascii_case("bytes")
            }
        }

        fn rows<S: Storage>(
            database: &crate::db::Database<S>,
            sql: &str,
        ) -> Result<Vec<Vec<DataValue>>, DatabaseError> {
            let mut iter = database
                .run(sql)
                .inspect_err(|err| eprintln!("{sql}: {err}"))?;
            let mut rows = Vec::new();
            while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
                rows.push(row);
            }
            iter.done()?;
            Ok(rows)
        }
        fn texts(rows: Vec<Vec<DataValue>>) -> Vec<String> {
            rows.into_iter()
                .map(|row| {
                    row.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect()
        }
        let open = |dir: &std::path::Path, folding: bool| {
            let builder = DataBaseBuilder::path(dir);
            if folding {
                builder
                    .register_dialect(Arc::new(CaseFoldingDialect {
                        inner: crate::dialect::GenericSqlDialect::new(),
                    }))
                    .default_dialect("case_folding_test")?
                    .build_lmdb()
            } else {
                builder.build_lmdb()
            }
        };

        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        // A table created byte-wise, before the collation.
        {
            let mut database = open(temp_dir.path(), false)?;
            database.ddl("create table legacy(id varchar(10) primary key, n int)")?;
            database
                .run("insert into legacy values ('Smith', 1), ('smith', 2)")?
                .done()?;
        }

        let mut database = open(temp_dir.path(), true)?;
        database.load(CatalogKind::Table("legacy".to_string()))?;
        database
            .ddl("create table people(id int primary key, name varchar(20) unique, tag varchar(10) collate bytes)")?;
        database
            .run("insert into people values (1, 'Smith', 'a'), (2, 'jones', 'A')")?
            .done()?;
        // The unique key holds the folded name.
        let err = database
            .run("insert into people values (3, 'SMITH', 'b')")
            .and_then(|iter| iter.done())
            .unwrap_err();
        assert!(matches!(err, DatabaseError::DuplicateUniqueValue), "{err}");
        assert_eq!(
            texts(rows(
                &database,
                "select id from people where name = 'SMITH'"
            )?),
            ["1"]
        );
        assert_eq!(
            texts(rows(
                &database,
                "select id from people where name in ('JONES', 'x')"
            )?),
            ["2"]
        );
        assert_eq!(
            texts(rows(&database, "select 'a' = 'A', 'B' > 'a'")?),
            ["true,true"]
        );
        assert_eq!(
            texts(rows(&database, "select name from people order by name")?),
            ["jones", "Smith"]
        );
        // A byte-wise column compares and sorts by bytes.
        assert_eq!(
            texts(rows(&database, "select id from people where tag = 'a'")?),
            ["1"]
        );
        assert_eq!(
            texts(rows(&database, "select tag from people order by tag")?),
            ["A", "a"]
        );
        assert_eq!(
            texts(rows(
                &database,
                "select upper(name), count(*) from people group by upper(name) order by 1"
            )?),
            ["JONES,1", "SMITH,1"]
        );
        database.ddl("create table dup(id int primary key, t varchar(10))")?;
        database
            .run("insert into dup values (1, 'x'), (2, 'X'), (3, 'y')")?
            .done()?;
        assert_eq!(
            texts(rows(&database, "select count(distinct t) from dup")?),
            ["2"]
        );
        // The byte-wise table keeps both keys and still finds each row.
        assert_eq!(
            texts(rows(&database, "select n from legacy order by n")?),
            ["1", "2"]
        );
        // Its key lookups stay byte-wise until the table is recreated.
        assert_eq!(
            texts(rows(&database, "select n from legacy where id = 'smith'")?),
            ["2"]
        );
        drop(database);

        // A table created under the collation is refused without it.
        let mut database = open(temp_dir.path(), false)?;
        database.load(CatalogKind::Table("people".to_string()))?;
        let err = database
            .run("select * from people")
            .and_then(|iter| iter.done())
            .unwrap_err();
        assert!(err.to_string().contains("lower"), "{err}");

        Ok(())
    }

    #[test]
    fn test_update_rejects_null_in_not_null_column() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory()?;
        database.ddl("create table t_upd_nn(id int primary key, b int not null, c int)")?;
        database
            .run("insert into t_upd_nn values (1, 1, 1), (2, 2, null)")?
            .done()?;

        for sql in [
            "update t_upd_nn set b = null where id = 1",
            "update t_upd_nn set b = c where id = 2",
            "update t_upd_nn set b = c + null",
        ] {
            let err = database.run(sql).and_then(|iter| iter.done()).unwrap_err();
            assert!(
                matches!(&err, DatabaseError::NotNull { column: Some(c), .. } if c == "b"),
                "{sql}: {err}"
            );
        }
        // The failed statements changed nothing; a nullable column still takes NULL.
        database
            .run("update t_upd_nn set c = null where id = 1")?
            .done()?;
        let mut iter = database.run("select id, b, c from t_upd_nn order by id")?;
        let mut rows = Vec::new();
        while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
            rows.push(row);
        }
        iter.done()?;
        assert_eq!(
            rows,
            vec![
                vec![DataValue::Int32(1), DataValue::Int32(1), DataValue::Null],
                vec![DataValue::Int32(2), DataValue::Int32(2), DataValue::Null],
            ]
        );

        Ok(())
    }

    #[test]
    fn test_insert_rejects_duplicate_target_column() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory()?;
        database.ddl("create table t_ins_dup(id int primary key, a int, b int)")?;

        for sql in [
            "insert into t_ins_dup (id, a, a) values (1, 1, 2)",
            "insert into t_ins_dup (id, a, b, b) values (1, 1, 2, 3)",
            "insert into t_ins_dup (id, a, a) select 1, 2, 3",
        ] {
            let err = database.run(sql).and_then(|iter| iter.done()).unwrap_err();
            assert!(
                matches!(&err, DatabaseError::DuplicateTargetColumn(c) if c == "a" || c == "b"),
                "{sql}: {err}"
            );
        }
        let mut iter = database.run("select count(*) from t_ins_dup")?;
        let row = iter.next_tuple(|_, row| row.take_values())?.unwrap();
        iter.done()?;
        assert_eq!(row, vec![DataValue::Int32(0)]);

        Ok(())
    }
}
// GRCOV_EXCL_STOP
