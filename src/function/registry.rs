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
use crate::expression::function::table::{
    ArcTableFunctionImpl, TableFunctionCatalog, TableFunctionImpl,
};
use crate::expression::function::FunctionSummary;
use crate::types::LogicalType;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Key for tracking built-in functions, ignoring persistence policy
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FunctionKey {
    name: Arc<str>,
    arg_types: Vec<LogicalType>,
}

impl From<&FunctionSummary> for FunctionKey {
    fn from(summary: &FunctionSummary) -> Self {
        FunctionKey {
            name: summary.name.clone(),
            arg_types: summary.arg_types.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FunctionRegistry {
    scalar_functions: HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>>,
    pub(crate) table_functions: HashMap<FunctionSummary, Arc<TableFunctionCatalog>>,
    builtin_functions: HashSet<FunctionKey>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn mark_builtins_loaded(&mut self) {
        self.builtin_functions = self
            .scalar_functions
            .keys()
            .map(FunctionKey::from)
            .collect();
    }

    pub fn register_scalar(
        &mut self,
        function: Arc<dyn ScalarFunctionImpl>,
    ) -> Result<(), DatabaseError> {
        let summary = function.summary().clone();
        let key = FunctionKey::from(&summary);
        // Allow overriding built-ins, but not user-registered functions
        let is_builtin = self.builtin_functions.contains(&key);
        let exists = self.scalar_functions.contains_key(&summary);

        if exists && !is_builtin {
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

        if scored_candidates.len() > 1 && scored_candidates[0].0 == scored_candidates[1].0 {
            let candidates: Vec<String> = scored_candidates
                .iter()
                .filter(|(q, _)| *q == scored_candidates[0].0)
                .map(|(_, func)| {
                    let summary = func.summary();
                    format!(
                        "{}({})",
                        summary.name,
                        summary
                            .arg_types
                            .iter()
                            .map(|t| t.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect();
            return Err(DatabaseError::AmbiguousFunction {
                name: name.to_string(),
                candidates,
            });
        }

        Ok(scored_candidates[0].1.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CatalogKind, DataBaseBuilder};
    use crate::errors::DatabaseError;
    use crate::expression::function::scala::ScalarFunctionImpl;
    use crate::expression::function::{FunctionSummary, PersistencePolicy};
    use crate::types::value::DataValue;
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
    struct CustomAbsFunction {
        summary: FunctionSummary,
        return_type: LogicalType,
    }

    impl CustomAbsFunction {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "abs".into(),
                    arg_types: vec![LogicalType::Integer],
                    persistence: PersistencePolicy::Durable,
                },
                return_type: LogicalType::Integer,
            })
        }
    }

    impl ScalarFunctionImpl for CustomAbsFunction {
        fn eval(
            &self,
            args: &[crate::planner::ExprRef],
            arena: &(dyn crate::planner::MetaArena + '_),
            tuple: Option<&dyn crate::types::tuple::Input>,
        ) -> Result<DataValue, DatabaseError> {
            let value = arena.expression(args[0]).eval(arena, tuple)?;
            Ok(match value.as_ref() {
                DataValue::Int32(v) => DataValue::Int32(1000 + v.abs()),
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
            None
        }
    }

    #[derive(Debug, Clone)]
    struct DoubleArgFunction {
        summary: FunctionSummary,
        return_type: LogicalType,
    }

    impl DoubleArgFunction {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "double_arg".into(),
                    arg_types: vec![LogicalType::Double],
                    persistence: PersistencePolicy::Transient,
                },
                return_type: LogicalType::Double,
            })
        }
    }

    impl ScalarFunctionImpl for DoubleArgFunction {
        fn eval(
            &self,
            args: &[crate::planner::ExprRef],
            arena: &(dyn crate::planner::MetaArena + '_),
            tuple: Option<&dyn crate::types::tuple::Input>,
        ) -> Result<DataValue, DatabaseError> {
            use ordered_float::OrderedFloat;
            let value = arena.expression(args[0]).eval(arena, tuple)?;
            // Cast to Double if needed (demonstrates implicit widening)
            let double_value = value.into_owned().cast(&LogicalType::Double)?;
            Ok(match &double_value {
                DataValue::Float64(v) => DataValue::Float64(OrderedFloat(v.0 * 2.0)),
                _ => double_value,
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

    fn next_tuple_owned(
        iter: &mut impl crate::db::ResultIter,
    ) -> Result<Option<crate::types::tuple::TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone())
    }

    #[test]
    fn test_register_and_call_scalar_udf() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .build_in_memory()?;

        let mut iter = database.run("select add_one(41)")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(42));
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_builtin_abs_works() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path()).build_in_memory()?;

        let mut iter = database.run("select abs(CAST(-5 AS INTEGER))")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(5));
        iter.done()?;
        Ok(())
    }

    #[test]
    fn test_override_builtin_function() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(CustomAbsFunction::new())
            .build_in_memory()?;

        // Verify the custom abs function is called (returns 1000 + abs(value))
        // Use explicit INTEGER cast to ensure we hit the Integer overload
        let mut iter = database.run("select abs(CAST(-5 AS INTEGER))")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(1005));
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_duplicate_function_error() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let result = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .register_scalar_function(AddOneFunction::new())
            .build_in_memory();

        assert!(result.is_err());
        match result {
            Err(DatabaseError::FunctionAlreadyExists(name)) => {
                assert!(name.contains("add_one"));
            }
            _ => panic!("Expected FunctionAlreadyExists error"),
        }

        Ok(())
    }

    #[test]
    fn test_overload_resolution_with_widening() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(DoubleArgFunction::new())
            .build_in_memory()?;

        let mut iter = database.run("select double_arg(21)")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        use ordered_float::OrderedFloat;
        assert_eq!(tuple[0], DataValue::Float64(OrderedFloat(42.0)));
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_null_literal_resolution() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .build_in_memory()?;

        let mut iter = database.run("select add_one(NULL)")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Null);
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_two_overloads_work() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .register_scalar_function(AddOneInt64Function::new())
            .build_in_memory()?;

        // Test Integer overload
        let mut iter = database.run("select add_one(CAST(41 AS INTEGER))")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(42));
        iter.done()?;

        // Test Bigint overload
        let mut iter = database.run("select add_one(CAST(41 AS BIGINT))")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int64(42));
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_ambiguous_overload_error() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_scalar_function(AddOneFunction::new())
            .register_scalar_function(AddOneInt64Function::new())
            .build_in_memory()?;

        // NULL literal creates a genuine tie: both Integer and Bigint overloads
        // match with NullLiteral quality, so we should get AmbiguousFunction
        let result = database.run("select add_one(NULL)");
        assert!(result.is_err());
        match result {
            Err(DatabaseError::AmbiguousFunction { name, candidates }) => {
                assert_eq!(name, "add_one");
                assert_eq!(candidates.len(), 2);
            }
            Err(ref e) => panic!("Expected AmbiguousFunction error, got: {}", e),
            Ok(_) => panic!("Expected error but query succeeded"),
        }

        Ok(())
    }

    #[derive(Debug)]
    struct RangeTableFunction {
        summary: FunctionSummary,
    }

    impl RangeTableFunction {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "range".into(),
                    arg_types: vec![],
                    persistence: PersistencePolicy::Durable,
                },
            })
        }
    }

    impl crate::expression::function::table::TableFunctionImpl for RangeTableFunction {
        fn eval(
            &self,
            _args: &[crate::planner::ExprRef],
            _arena: &(dyn crate::planner::MetaArena + '_),
        ) -> Result<crate::expression::function::table::TableFunctionIter, DatabaseError> {
            Ok(Box::new((0..5).map(|i| {
                Ok(crate::types::tuple::Tuple::new(
                    None,
                    vec![DataValue::Int32(i)],
                ))
            }))
                as crate::expression::function::table::TableFunctionIter)
        }

        fn summary(&self) -> &FunctionSummary {
            &self.summary
        }

        fn output_schema_into(
            &self,
            table_arena: &mut crate::planner::TableArena,
            schema: &mut crate::types::tuple::Schema,
        ) {
            use crate::catalog::{ColumnCatalog, ColumnDesc};
            schema.push(table_arena.alloc_column(ColumnCatalog::new(
                "n".to_string(),
                true,
                ColumnDesc::new(LogicalType::Integer, None, false, None).unwrap(),
            )));
        }
    }

    #[test]
    fn test_table_function() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_table_function(RangeTableFunction::new())
            .build_in_memory()?;

        // Use TABLE keyword for table function calls
        let mut iter = database.run("select * from table(range())")?;
        let mut count = 0;
        while let Some(tuple) = next_tuple_owned(&mut iter)? {
            assert_eq!(tuple[0], DataValue::Int32(count));
            count += 1;
        }
        assert_eq!(count, 5);
        iter.done()?;

        Ok(())
    }

    #[derive(Debug, Clone)]
    struct DurableUdfFunction {
        summary: FunctionSummary,
        return_type: LogicalType,
    }

    impl DurableUdfFunction {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                summary: FunctionSummary {
                    name: "durable_add".into(),
                    arg_types: vec![LogicalType::Integer],
                    persistence: PersistencePolicy::Durable,
                },
                return_type: LogicalType::Integer,
            })
        }
    }

    impl ScalarFunctionImpl for DurableUdfFunction {
        fn eval(
            &self,
            args: &[crate::planner::ExprRef],
            arena: &(dyn crate::planner::MetaArena + '_),
            tuple: Option<&dyn crate::types::tuple::Input>,
        ) -> Result<DataValue, DatabaseError> {
            let value = arena.expression(args[0]).eval(arena, tuple)?;
            Ok(match value.as_ref() {
                DataValue::Int32(v) => DataValue::Int32(v + 100),
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

    fn next_tuple_owned_sync(
        iter: &mut impl crate::db::ResultIter,
    ) -> Result<Option<crate::types::tuple::TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone())
    }

    // TODO: LMDB persistence test skipped - requires async/sync API alignment
    // The test should validate that Durable functions persist across database reopens
    // but currently requires proper ddl() method access in sync LMDB mode
    #[test]
    #[ignore]
    #[cfg(feature = "lmdb")]
    fn test_durable_function_persistence() {
        // Test implementation pending async/sync API resolution for LMDB
    }
}
