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

use crate::errors::DatabaseError;
use crate::expression::function::scala::ScalarFunctionImpl;
use crate::expression::function::table::TableFunctionCatalog;
use crate::expression::function::FunctionSummary;
use crate::types::LogicalType;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct FunctionRegistry {
    scalar_functions: HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>>,
    pub(crate) table_functions: HashMap<FunctionSummary, Arc<TableFunctionCatalog>>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_scalar(
        &mut self,
        function: Arc<dyn ScalarFunctionImpl>,
    ) -> Result<(), DatabaseError> {
        self.register_scalar_with_options(function, false)
    }

    pub fn register_scalar_with_options(
        &mut self,
        function: Arc<dyn ScalarFunctionImpl>,
        allow_override: bool,
    ) -> Result<(), DatabaseError> {
        let summary = function.summary().clone();
        if !allow_override && self.scalar_functions.contains_key(&summary) {
            return Err(DatabaseError::FunctionAlreadyExists(
                summary.name.to_string(),
            ));
        }
        self.scalar_functions.insert(summary, function);
        Ok(())
    }

    pub fn register_table(
        &mut self,
        catalog: Arc<TableFunctionCatalog>,
    ) -> Result<(), DatabaseError> {
        self.register_table_with_options(catalog, false)
    }

    pub fn register_table_with_options(
        &mut self,
        catalog: Arc<TableFunctionCatalog>,
        allow_override: bool,
    ) -> Result<(), DatabaseError> {
        let summary = catalog.inner.summary().clone();
        if !allow_override && self.table_functions.contains_key(&summary) {
            return Err(DatabaseError::FunctionAlreadyExists(
                summary.name.to_string(),
            ));
        }
        self.table_functions.insert(summary, catalog);
        Ok(())
    }

    pub fn get_scalar(&self, summary: &FunctionSummary) -> Option<&Arc<dyn ScalarFunctionImpl>> {
        self.scalar_functions.get(summary)
    }

    pub fn get_table(&self, summary: &FunctionSummary) -> Option<&Arc<TableFunctionCatalog>> {
        self.table_functions.get(summary)
    }

    pub fn scalar_functions(&self) -> &HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>> {
        &self.scalar_functions
    }

    pub fn table_functions(&self) -> &HashMap<FunctionSummary, Arc<TableFunctionCatalog>> {
        &self.table_functions
    }

    pub(crate) fn load_scalar(&mut self, function: Arc<dyn ScalarFunctionImpl>) {
        let summary = function.summary().clone();
        self.scalar_functions.insert(summary, function);
    }

    pub(crate) fn load_table(&mut self, catalog: Arc<TableFunctionCatalog>) {
        let summary = catalog.inner.summary().clone();
        self.table_functions.insert(summary, catalog);
    }

    pub fn resolve_scalar(
        &self,
        name: &str,
        arg_types: &[LogicalType],
    ) -> Result<Arc<dyn ScalarFunctionImpl>, DatabaseError> {
        let candidates: Vec<_> = self
            .scalar_functions
            .iter()
            .filter(|(summary, _)| summary.name.as_ref() == name)
            .filter(|(summary, _)| summary.arg_types.len() == arg_types.len())
            .collect();

        if candidates.is_empty() {
            return Err(DatabaseError::function_not_found(name));
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum MatchQuality {
            ExactMatch,
            ImplicitCast,
            NullLiteral,
        }

        let mut scored_candidates = Vec::new();

        for (summary, function) in candidates {
            let mut overall_quality = MatchQuality::ExactMatch;
            let mut compatible = true;

            for (provided, expected) in arg_types.iter().zip(summary.arg_types.iter()) {
                if provided == expected {
                    continue;
                } else if provided == &LogicalType::SqlNull {
                    overall_quality = overall_quality.max(MatchQuality::NullLiteral);
                } else if LogicalType::can_implicit_cast(provided, expected) {
                    overall_quality = overall_quality.max(MatchQuality::ImplicitCast);
                } else {
                    compatible = false;
                    break;
                }
            }

            if compatible {
                scored_candidates.push((overall_quality, function.clone()));
            }
        }

        if scored_candidates.is_empty() {
            return Err(DatabaseError::function_not_found(format!(
                "{} with argument types {:?}",
                name, arg_types
            )));
        }

        scored_candidates.sort_by_key(|(quality, _)| *quality);

        Ok(scored_candidates[0].1.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::DEFAULT_SCHEMA_ID;
    use crate::db::{DataBaseBuilder, Database};
    use crate::errors::DatabaseError;
    use crate::expression::function::scala::ScalarFunctionImpl;
    use crate::expression::function::{FunctionSummary, PersistencePolicy};
    use crate::types::value::{DataValue, ValueRef};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[derive(Debug, Clone)]
    struct AddOneFunction {
        summary: FunctionSummary,
        return_type: LogicalType,
    }

    impl AddOneFunction {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "add_one".into(),
                    arg_types: vec![LogicalType::Integer],
                    persistence: PersistencePolicy::Transient,
                },
                return_type: LogicalType::Integer,
            })
        }
    }

    impl ScalarFunctionImpl for AddOneFunction {
        fn eval(
            &self,
            args: &[crate::planner::ExprRef],
            arena: &(dyn crate::planner::MetaArena + '_),
            tuple: Option<&dyn crate::types::tuple::Input>,
        ) -> Result<DataValue, DatabaseError> {
            let value = arena.expression(args[0]).eval(arena, tuple)?;
            Ok(match value.as_ref() {
                DataValue::Int32(v) => DataValue::Int32(v + 1),
                _ => value.into_owned(),
            })
        }

        fn return_type(&self) -> &LogicalType {
            &self.return_type
        }

        fn summary(&self) -> &FunctionSummary {
            &self.summary
        }

        fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
            Some(vec![Some(true)])
        }
    }

    #[derive(Debug, Clone)]
    struct AddOneInt64Function {
        summary: FunctionSummary,
        return_type: LogicalType,
    }

    impl AddOneInt64Function {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "add_one".into(),
                    arg_types: vec![LogicalType::Bigint],
                    persistence: PersistencePolicy::Transient,
                },
                return_type: LogicalType::Bigint,
            })
        }
    }

    impl ScalarFunctionImpl for AddOneInt64Function {
        fn eval(
            &self,
            args: &[crate::planner::ExprRef],
            arena: &(dyn crate::planner::MetaArena + '_),
            tuple: Option<&dyn crate::types::tuple::Input>,
        ) -> Result<DataValue, DatabaseError> {
            let value = arena.expression(args[0]).eval(arena, tuple)?;
            Ok(match value.as_ref() {
                DataValue::Int64(v) => DataValue::Int64(v + 1),
                _ => value.into_owned(),
            })
        }

        fn return_type(&self) -> &LogicalType {
            &self.return_type
        }

        fn summary(&self) -> &FunctionSummary {
            &self.summary
        }

        fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
            Some(vec![Some(true)])
        }
    }

    #[derive(Debug, Clone)]
    struct CustomAbsFunction;

    impl ScalarFunctionImpl for CustomAbsFunction {
        fn eval(
            &self,
            _exprs: &[crate::expression::ScalarExpression],
            tuples: &[crate::types::tuple::Tuple],
        ) -> Result<DataValue, DatabaseError> {
            if tuples.is_empty() || tuples[0].values.is_empty() {
                return Ok(DataValue::Null);
            }
            match &tuples[0].values[0] {
                DataValue::Int32(v) => Ok(DataValue::Int32(1000 + v.abs())),
                _ => Ok(DataValue::Null),
            }
        }

        fn summary(&self) -> &FunctionSummary {
            static SUMMARY: std::sync::OnceLock<FunctionSummary> = std::sync::OnceLock::new();
            SUMMARY.get_or_init(|| FunctionSummary {
                name: "abs".into(),
                arg_types: vec![LogicalType::Integer],
                persistence: PersistencePolicy::Transient,
            })
        }

        fn monotonicity(&self) -> Option<crate::optimizer::core::opt_type::Monotonicity> {
            None
        }
    }

    #[derive(Debug, Clone)]
    struct DoubleArgFunction;

    impl ScalarFunctionImpl for DoubleArgFunction {
        fn eval(
            &self,
            _exprs: &[crate::expression::ScalarExpression],
            tuples: &[crate::types::tuple::Tuple],
        ) -> Result<DataValue, DatabaseError> {
            if tuples.is_empty() || tuples[0].values.is_empty() {
                return Ok(DataValue::Null);
            }
            match &tuples[0].values[0] {
                DataValue::Float64(v) => Ok(DataValue::Float64(v * 2.0)),
                _ => Ok(DataValue::Null),
            }
        }

        fn summary(&self) -> &FunctionSummary {
            static SUMMARY: std::sync::OnceLock<FunctionSummary> = std::sync::OnceLock::new();
            SUMMARY.get_or_init(|| FunctionSummary {
                name: "double_arg".into(),
                arg_types: vec![LogicalType::Double],
                persistence: PersistencePolicy::Transient,
            })
        }

        fn monotonicity(&self) -> Option<crate::optimizer::core::opt_type::Monotonicity> {
            Some(crate::optimizer::core::opt_type::Monotonicity::Increasing)
        }
    }

    async fn next_tuple_owned(
        iter: &mut impl crate::db::ResultIter,
    ) -> Result<Option<crate::types::tuple::TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone()).await
    }

    #[tokio::test]
    async fn test_register_and_call_scalar_udf() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .build_in_memory()
            .await?;

        let mut iter = database.run("select add_one(41)").await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(42));
        iter.done().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_override_builtin_function() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(Arc::new(CustomAbsFunction))
            .build_in_memory()
            .await?;

        let mut iter = database.run("select abs(-5)").await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(1005));
        iter.done().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_duplicate_function_error() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let result = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(Arc::new(AddOneFunction))
            .register_scalar_function(Arc::new(AddOneFunction))
            .build_in_memory()
            .await;

        assert!(result.is_err());
        match result {
            Err(DatabaseError::FunctionAlreadyExists(name)) => {
                assert!(name.contains("add_one"));
            }
            _ => panic!("Expected FunctionAlreadyExists error"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_overload_resolution_with_widening() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(Arc::new(DoubleArgFunction))
            .build_in_memory()
            .await?;

        let mut iter = database.run("select double_arg(21)").await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        use ordered_float::OrderedFloat;
        assert_eq!(tuple[0], DataValue::Float64(OrderedFloat(42.0)));
        iter.done().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_null_literal_resolution() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(Arc::new(AddOneFunction))
            .build_in_memory()
            .await?;

        let mut iter = database.run("select add_one(NULL)").await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Null);
        iter.done().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_ambiguous_overload_error() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(Arc::new(AddOneFunction))
            .register_scalar_function(Arc::new(AddOneInt64Function))
            .build_in_memory()
            .await?;

        let result = database.run("select add_one(NULL)").await;
        assert!(result.is_err());
        match result {
            Err(DatabaseError::AmbiguousFunction(_)) => {}
            _ => panic!("Expected AmbiguousFunction error"),
        }

        Ok(())
    }

    #[derive(Debug, Clone)]
    struct RangeTableFunction;

    impl crate::expression::function::table::TableFunctionImpl for RangeTableFunction {
        fn return_type(&self) -> &crate::catalog::TableCatalog {
            use crate::catalog::{ColumnCatalog, ColumnDesc};
            use crate::types::ColumnId;

            static TABLE_CATALOG: std::sync::OnceLock<crate::catalog::TableCatalog> =
                std::sync::OnceLock::new();
            TABLE_CATALOG.get_or_init(|| {
                let mut catalog = crate::catalog::TableCatalog::new(
                    Arc::new("range".to_string()),
                    vec![Arc::new(ColumnCatalog::new(
                        "n".to_string(),
                        false,
                        ColumnDesc::new(LogicalType::Integer, None, false, None),
                        Some(ColumnId::default()),
                    ))],
                )
                .unwrap();
                catalog.schema_id = Some(crate::catalog::DEFAULT_SCHEMA_ID);
                catalog
            })
        }

        fn execute(
            &self,
            _args: &[crate::expression::ScalarExpression],
            _arg_values: &[crate::types::value::ValueRef],
        ) -> Result<Vec<crate::types::tuple::Tuple>, DatabaseError> {
            Ok((0..5)
                .map(|i| crate::types::tuple::Tuple::new(None, vec![DataValue::Int32(i)]))
                .collect())
        }

        fn summary(&self) -> &crate::expression::function::FunctionSummary {
            static SUMMARY: std::sync::OnceLock<FunctionSummary> = std::sync::OnceLock::new();
            SUMMARY.get_or_init(|| FunctionSummary {
                name: "range".into(),
                arg_types: vec![],
                persistence: PersistencePolicy::Transient,
            })
        }
    }

    #[tokio::test]
    async fn test_table_function() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_table_function(Arc::new(RangeTableFunction))
            .build_in_memory()
            .await?;

        let mut iter = database.run("select * from range()").await?;
        let mut count = 0;
        while let Some(tuple) = next_tuple_owned(&mut iter).await? {
            assert_eq!(tuple[0], DataValue::Int32(count));
            count += 1;
        }
        assert_eq!(count, 5);
        iter.done().await?;

        Ok(())
    }

    #[derive(Debug, Clone)]
    struct DurableUdfFunction;

    impl ScalarFunctionImpl for DurableUdfFunction {
        fn eval(
            &self,
            _exprs: &[crate::expression::ScalarExpression],
            tuples: &[crate::types::tuple::Tuple],
        ) -> Result<DataValue, DatabaseError> {
            if tuples.is_empty() || tuples[0].values.is_empty() {
                return Ok(DataValue::Null);
            }
            match &tuples[0].values[0] {
                DataValue::Int32(v) => Ok(DataValue::Int32(v + 100)),
                _ => Ok(DataValue::Null),
            }
        }

        fn summary(&self) -> &FunctionSummary {
            static SUMMARY: std::sync::OnceLock<FunctionSummary> = std::sync::OnceLock::new();
            SUMMARY.get_or_init(|| FunctionSummary {
                name: "durable_add".into(),
                arg_types: vec![LogicalType::Integer],
                persistence: PersistencePolicy::Durable,
            })
        }

        fn monotonicity(&self) -> Option<crate::optimizer::core::opt_type::Monotonicity> {
            Some(crate::optimizer::core::opt_type::Monotonicity::Increasing)
        }
    }

    fn next_tuple_owned_sync(
        iter: &mut impl crate::db::ResultIter,
    ) -> Result<Option<crate::types::tuple::TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone())
    }

    #[test]
    #[cfg(feature = "lmdb")]
    fn test_durable_function_persistence() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let db_path = temp_dir.path();

        {
            let mut database = DataBaseBuilder::path(db_path)
                .register_scalar_function(Arc::new(DurableUdfFunction))
                .build_lmdb()?;
            database
                .run("create table t1(c1 int primary key, c2 int default durable_add(10))")?
                .done()?;
            database
                .run("insert into t1(c1) values (1)")?
                .done()?;
            database
                .run("create view v1 as select durable_add(c1) as result from t1")?
                .done()?;

            let mut iter = database.run("select * from v1")?;
            let tuple = next_tuple_owned_sync(&mut iter)?.unwrap();
            assert_eq!(tuple[0], DataValue::Int32(101));
            iter.done()?;
        }

        {
            let mut database = DataBaseBuilder::path(db_path)
                .register_scalar_function(Arc::new(DurableUdfFunction))
                .build_lmdb()?;
            database.load(crate::catalog::CatalogKind::Table("t1".into()))?;
            database.load(crate::catalog::CatalogKind::View("v1".into()))?;

            let mut iter = database.run("select * from v1")?;
            let tuple = next_tuple_owned_sync(&mut iter)?.unwrap();
            assert_eq!(tuple[0], DataValue::Int32(101));
            iter.done()?;

            let mut iter = database.run("select c2 from t1")?;
            let tuple = next_tuple_owned_sync(&mut iter)?.unwrap();
            assert_eq!(tuple[0], DataValue::Int32(110));
            iter.done()?;
        }

        Ok(())
    }
}
