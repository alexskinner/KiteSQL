## Features
### Shell

Run the local interactive shell with:

```bash
cargo run --bin kitesql-shell
```

Use a custom data directory:

```bash
cargo run --bin kitesql-shell -- --path ./tmp/kitesql-shell-data
```

Run a one-shot SQL check:

```bash
cargo run --bin kitesql-shell -- -e "select current_timestamp"
```

In interactive mode, end SQL statements with `;`; an empty line also executes the buffered statement.

Built-in metacommands:

- `.help`
- `.quit`
- `.tables`
- `.views`
- `.schema <name>`

### PG Wire: 

run `cargo run --features="net"` to start service

### SQL Dialects

KiteSQL supports pluggable SQL dialects through the `SqlDialect` trait. By default, KiteSQL uses a generic SQL dialect, but you can register custom dialects to support database-specific syntax and type mappings.

#### Using MySQL Dialect

The `kite_sql_mysql` crate provides full MySQL dialect support:

```rust
use kite_sql::db::DataBaseBuilder;
use kite_sql_mysql::MySqlBuilderExt;

let database = DataBaseBuilder::path(".")
    .with_mysql()?
    .build_in_memory()?;

// Now you can use MySQL-specific types
database.run("CREATE TABLE users (id SERIAL PRIMARY KEY, bio MEDIUMTEXT)")?;
```

#### Writing Custom Dialects

You can write your own dialect by implementing the `SqlDialect` trait:

```rust
use kite_sql::dialect::SqlDialect;
use kite_sql::types::LogicalType;
use kite_sql::errors::DatabaseError;
use sqlparser::ast::DataType;
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect};
use std::sync::Arc;

#[derive(Debug)]
struct MyDialect {
    parser: GenericDialect,
}

impl SqlDialect for MyDialect {
    fn name(&self) -> &str {
        "my_dialect"
    }

    fn parser_dialect(&self) -> &dyn SqlParserDialect {
        &self.parser
    }

    // Map custom types to KiteSQL's logical types
    fn map_data_type(
        &self,
        data_type: &DataType,
    ) -> Option<Result<LogicalType, DatabaseError>> {
        match data_type {
            DataType::Custom(name, _) if name.0.len() == 1 => {
                // Handle your custom types here
                None
            }
            _ => None,
        }
    }
}

// Register and use your dialect
let my_dialect = Arc::new(MyDialect { parser: GenericDialect {} });
let database = DataBaseBuilder::path(".")
    .register_dialect(my_dialect)
    .default_dialect("my_dialect")?
    .build_in_memory()?;
```

The `map_data_type` hook is called first, allowing your dialect to handle types before falling back to the generic mapping. Return `Some(Ok(logical_type))` to map a type, `Some(Err(...))` to reject it with an error, or `None` to fall back to the default mapping.

### ORM Mapping: `features = ["orm"]`
See [the ORM guide](../src/orm/README.md) for the full ORM guide, including:

- `#[derive(Model)]` usage and supported attributes
- CRUD, DDL, and migration helpers
- typed query builder APIs
- public ORM structs, enums, and traits
- related `ResultIter::orm::<M>()` integration

### Async API on S3: `features = ["slatedb-s3"]`
With the `async` feature, the storage traits, the executor and the database API compile as async from the same source; the sync build is unchanged. The `slatedb-s3` feature adds `SlateDbStorage`, which keeps the data in an S3 bucket through [SlateDB](https://slatedb.io), an LSM tree on object storage, and runs on tokio. Any process that opens the bucket path sees the same database.

```rust
use kite_sql::db::{AsyncIter, DataBaseBuilder};
use kite_sql::storage::slatedb::{S3Config, SlateDbStorage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Credentials and region come from the `AWS_*` environment variables.
    let config = S3Config::new("my-bucket");
    let storage = SlateDbStorage::open_s3("kitesql/demo", &config, Default::default()).await?;
    let mut database = DataBaseBuilder::path(".").build_with_storage(storage.clone())?;

    database.migrate::<User>().await?;
    database.insert(&User { id: 1, name: "Alice".to_string(), age: Some(18) }).await?;
    let mut users = database.fetch::<User>().await?;
    while let Some(user) = users.next().await {
        println!("{:?}", user?);
    }
    users.done().await?;

    drop(database);
    // Flushes what is still in memory to S3.
    storage.close().await?;
    Ok(())
}
```

`async` excludes the sync storages (`rocksdb`, `lmdb`) and the shell, Python and Wasm bindings: pick one path per build. The full example is [async_s3](../examples/async_s3.rs):
```bash
KITESQL_S3_BUCKET=my-bucket AWS_REGION=us-east-1 AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... \
cargo run --example async_s3 --no-default-features --features slatedb-s3,orm
```

A reopened database loads its tables with `database.load(CatalogKind::Table(name))` before
querying them, as `async_s3` does.

By default a commit waits until its write is durable in S3. `storage.await_durable(false)` makes
it return once the write is applied in memory; a crash may then lose the latest commits.

Reads that miss the block cache cost an S3 request each, so the cache size decides how fast
queries run. `SlateDbCacheOptions` sizes the memory cache and adds a local disk tier:
```rust
use kite_sql::storage::slatedb::{S3Config, SlateDbCacheOptions, SlateDbDiskCache, SlateDbStorage};

let cache = SlateDbCacheOptions {
    memory_bytes: Some(4 << 30),
    disk: Some(SlateDbDiskCache {
        dir: "./slatedb-cache".into(),
        // The cache files grow as they fill, so a large capacity costs nothing up front.
        capacity_bytes: 64 << 30,
    }),
    // Keep the blocks compactions write, which otherwise leave the cache.
    cache_compaction_output: true,
};
let storage = SlateDbStorage::open_s3("kitesql/demo", &S3Config::new("my-bucket"), cache).await?;
```
`SlateDbScanOptions` sets how scans read SST blocks: 256KiB per request, 4 requests in flight and
cached blocks by default.

### Spill-backed Aggregation: `features = ["spill"]`

The native-only `spill` feature bounds memory usage for large grouped
aggregations. It is not enabled by default and cannot be combined with `wasm`.

For SQL, place the optimizer hint immediately after `SELECT`:

```sql
SELECT /*+ FORCE_AGG_SPILL */ user_id, COUNT(*)
FROM orders
GROUP BY user_id
ORDER BY user_id;
```

For ORM queries, enable both `orm` and `spill`, then call `force_spill()` before
building the projection and aggregate plan:

```rust,ignore
let grouped = database.bind(|ctx| {
    ctx.from::<Order>()?
        .force_spill()?
        .project_tuple(|e| {
            Ok(vec![e.column(Order::user_id())?, e.count_all()?])
        })?
        .group_by(|e| e.column(Order::user_id()))?
        .order_by(Order::user_id())?
        .finish()
})?;
```

KiteSQL still reuses an already ordered input when possible. Otherwise it adds
an external sort on the group keys and executes a streaming aggregate. The
hint is intended for grouped aggregates or `DISTINCT`; an aggregate without
group keys already uses constant-size accumulator state.

### Nested-loop Join Hint

Use `FORCE_NEST_LOOP_JOIN` to select the nested-loop implementation for joins
in the current `SELECT` query block:

```sql
SELECT /*+ FORCE_NEST_LOOP_JOIN */ orders.id, users.name
FROM orders
JOIN users ON orders.user_id = users.id;
```

ORM queries can select the same implementation before adding joins:

```rust,ignore
let joined = database.bind(|ctx| {
    ctx.from::<Order>()?
        .force_nested_loop()
        .inner_join::<User, _>(|e| {
            e.column(Order::user_id())?.eq(e.column(User::id())?)
        })?
        .finish()
})?;
```

It avoids Hash Join's build-side tuple materialization, but may perform
substantially more work on large inputs.

### User-Defined Function: `features = ["macros"]`
```rust
scala_function!(TestFunction::test(LogicalType::Integer, LogicalType::Integer) -> LogicalType::Integer => |v1: DataValue, v2: DataValue| {
    let plus_binary_evaluator = EvaluatorFactory::binary_create(LogicalType::Integer, BinaryOperator::Plus)?;
    let value = plus_binary_evaluator.binary_eval(&v1, &v2);

    let plus_unary_evaluator = EvaluatorFactory::unary_create(LogicalType::Integer, UnaryOperator::Minus)?;
    Ok(plus_unary_evaluator.unary_eval(&value))
});

let kite_sql = DataBaseBuilder::path("./data")
    .register_scalar_function(TestFunction::new())
    .build()?;
```

### User-Defined Table Function: `features = ["macros"]`
```rust
table_function!(MyTableFunction::test_numbers(LogicalType::Integer) -> [c1: LogicalType::Integer, c2: LogicalType::Integer] => (|v1: DataValue| {
    let num = v1.i32().unwrap();

    Ok(Box::new((0..num)
        .into_iter()
        .map(|i| Ok(Tuple::new(
            None,
            vec![
                DataValue::Int32(i),
                DataValue::Int32(i),
            ]
        )))) as TableFunctionIter)
}));
let kite_sql = DataBaseBuilder::path("./data")
   .register_table_function(MyTableFunction::new())
   .build()?;
```

### User-Defined Aggregate Function (UDAF)

UDAFs are registered through `DataBaseBuilder::register_aggregate_function`. Implement `AggregateFunctionImpl` for metadata (name, argument types, output type) and `Accumulator` for stateful aggregation logic.

**Name-clash policy**: A registered UDAF with the same name as a built-in aggregate (e.g. `sum`, `count`) overrides the built-in, following the same policy as scalar functions.

```rust
use kite_sql::db::DataBaseBuilder;
use kite_sql::expression::agg_function::{AggregateFunctionImpl, Accumulator, AggKind};
use kite_sql::types::value::DataValue;
use kite_sql::types::LogicalType;
use std::sync::Arc;

/// Example UDAF that multiplies values instead of summing them
struct ProductAggregate {
    arg_type: LogicalType,
}

impl ProductAggregate {
    fn new(arg_type: LogicalType) -> Arc<Self> {
        Arc::new(Self { arg_type })
    }
}

impl AggregateFunctionImpl for ProductAggregate {
    fn kind(&self) -> AggKind {
        // Return AggKind::Udf with name and argument types
        AggKind::Udf(kite_sql::binder::AggFnId {
            name: "product".to_string(),
            arg_types: vec![self.arg_type.clone()],
        })
    }

    fn arg_types(&self) -> &[LogicalType] {
        std::slice::from_ref(&self.arg_type)
    }

    fn output_type(&self) -> &LogicalType {
        &self.arg_type
    }

    fn create_accumulator(&self) -> Result<Box<dyn Accumulator>, kite_sql::errors::DatabaseError> {
        Ok(Box::new(ProductAccumulator {
            product: DataValue::Int32(1),
            has_values: false,
        }))
    }
}

/// Accumulator holds mutable state for one group
struct ProductAccumulator {
    product: DataValue,
    has_values: bool,
}

impl Accumulator for ProductAccumulator {
    fn update_value(&mut self, values: &[DataValue]) -> Result<(), kite_sql::errors::DatabaseError> {
        if let Some(value) = values.get(0) {
            if !value.is_null() {
                if self.has_values {
                    self.product = self.product.binary_op(value, &kite_sql::expression::BinaryOperator::Multiply)?;
                } else {
                    self.product = value.clone();
                    self.has_values = true;
                }
            }
        }
        Ok(())
    }

    fn evaluate(&self) -> Result<DataValue, kite_sql::errors::DatabaseError> {
        if self.has_values {
            Ok(self.product.clone())
        } else {
            Ok(DataValue::Null)
        }
    }

    fn merge(&mut self, _other: &dyn Accumulator) -> Result<(), kite_sql::errors::DatabaseError> {
        // Currently unused: no spill/parallel/distributed aggregation in place
        unimplemented!("merge is not yet used")
    }
}

let database = DataBaseBuilder::path("./data")
    .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
    .build()?;

// Use the UDAF in SQL
let mut iter = database.run("SELECT product(value) FROM my_table")?;
```

### Optimizer
- RBO
- CBO based on RBO(Physical Selection)

### Executor
- Volcano

### MVCC Transaction
- Pessimistic (Default)
- Optimistic
- Isolation level: repeatable read; every statement in a transaction reads the snapshot taken when it began

### Checkpoint
KiteSQL exposes checkpoint as a storage capability rather than a full backup workflow. A checkpoint only creates a consistent local snapshot directory; compressing, uploading, retaining, and pruning backups should stay in application code.

Support matrix:
- `build_optimistic()` supports `Database::checkpoint(...)` through RocksDB's safe checkpoint API.
- `build_rocksdb()` requires Cargo feature `unsafe_txdb_checkpoint`, which uses RocksDB's `TransactionDB` checkpoint API.
- `build_lmdb()` and `build_in_memory()` do not currently expose checkpoint support.

Opt in for `TransactionDB` checkpoint support:
```bash
cargo check --features unsafe_txdb_checkpoint
```

Minimal usage:
```rust
use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;

fn main() -> Result<(), DatabaseError> {
    let database = DataBaseBuilder::path("./data").build_rocksdb()?;

    database.checkpoint("./backup/checkpoint-2026-03-29")?;

    Ok(())
}
```

If `unsafe_txdb_checkpoint` is not enabled, `build_rocksdb()` returns an explicit error instead of attempting the experimental implementation.

### Field options
- [not] null
- unique
- primary key

### Supports index type
- PrimaryKey
- Unique
- Normal
- Composite

### Supports multiple primary key types
- Tinyint
- UTinyint
- Smallint
- USmallint
- Integer
- UInteger
- Bigint
- UBigint
- Char
- Varchar

### DDL
- Begin (Server only)
- Commit (Server only)
- Rollback (Server only)
- Create
    - [x] Table
    - [x] Index: Unique\Normal\Composite
    - [x] View
- Drop
    - [x] Table
    - [x] Index
      - Tips: `Drop Index table_name.index_name`
    - [x] View
- Alert
    - [x] Add Column
    - [x] Drop Column
- [x] Truncate

### DQL
- [x] Select
    - SeqScan
    - IndexScan
    - FunctionScan
- [x] Where
- [x] Distinct
- [x] Alias
- [x] Aggregation: 
  - count()
  - sum()
  - avg()
  - min()
  - max()
- [x] SubQuery[select/from/where]
- [x] Join: 
  - Inner
  - Left
  - Right
  - Full
  - Cross (Natural\Using)
- [x] Exists
- [x] Group By
- [x] Having
- [x] Window functions:
  - `row_number()`, `rank()`, `dense_rank()`
  - `count()`, `sum()`, `avg()`, `min()`, `max()` with `OVER`
  - `PARTITION BY` and window `ORDER BY` with the default frame
  - Explicit frames, named windows, and `QUALIFY` are not yet supported
- [x] Order By
- [x] Limit
- [x] Show Tables
- [x] Explain
- [x] Describe
- [x] Union
- [x] EXCEPT
- [x] INTERSECT

### DML
- [x] Insert
- [x] Insert Overwrite
- [x] Update
- [x] Delete
- [x] Analyze
- [x] Copy To
- [x] Copy From

### DataTypes
- Invalid
- SqlNull
- Boolean
- Tinyint
- UTinyint
- Smallint
- USmallint
- Integer
- UInteger
- Bigint
- UBigint
- Float
- Double
- Char
- Varchar
- Date
- DateTime
- Time
- TimeStamp
- Tuple
