// Integration test to verify FunctionPack can be implemented from outside the kite_sql crate

// The tests use `tempfile`, a native-only dev-dependency.
#![cfg(all(not(target_arch = "wasm32"), not(feature = "async")))]

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;
use kite_sql::expression::function::scala::ScalarFunctionImpl;
use kite_sql::expression::function::{FunctionSummary, PersistencePolicy};
use kite_sql::types::value::DataValue;
use kite_sql::types::LogicalType;
use kite_sql::{FunctionPack, FunctionRegistry};
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Debug)]
struct TestFunctionPack;

impl FunctionPack for TestFunctionPack {
    fn name(&self) -> &str {
        "test_pack"
    }

    fn install(&self, registry: &mut FunctionRegistry) -> Result<(), DatabaseError> {
        registry.register_scalar(TestPackFunction::new())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TestPackFunction {
    summary: FunctionSummary,
    return_type: LogicalType,
}

impl TestPackFunction {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "test_pack_fn".into(),
                arg_types: vec![LogicalType::Integer],
                persistence: PersistencePolicy::Transient,
            },
            return_type: LogicalType::Integer,
        })
    }
}

impl ScalarFunctionImpl for TestPackFunction {
    fn eval(
        &self,
        args: &[kite_sql::planner::ExprRef],
        arena: &(dyn kite_sql::planner::MetaArena + '_),
        tuple: Option<&dyn kite_sql::types::tuple::Input>,
    ) -> Result<DataValue, DatabaseError> {
        let value = arena.expression(args[0]).eval(arena, tuple)?;
        Ok(match value.as_ref() {
            DataValue::Int32(v) => DataValue::Int32(v * 10),
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

#[test]
fn test_external_function_pack() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let database = DataBaseBuilder::path(temp_dir.path())
        .with_function_pack(Arc::new(TestFunctionPack))
        .build_in_memory()?;

    let mut iter = database.run("SELECT test_pack_fn(5)")?;
    let tuple = iter
        .next_tuple(|_, tuple| tuple.clone())?
        .expect("expected one row");
    assert_eq!(tuple[0], DataValue::Int32(50));
    iter.done()?;

    Ok(())
}
