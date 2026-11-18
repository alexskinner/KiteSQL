// Integration test to verify AggregateFunctionImpl and Accumulator can be implemented from outside kite_sql

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;
use kite_sql::expression::agg::{AggFnId, AggKind};
use kite_sql::expression::agg_function::{Accumulator, AggregateFunctionImpl};
use kite_sql::types::value::DataValue;
use kite_sql::types::LogicalType;
use std::fmt;
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Debug)]
struct ProductAggregate {
    input_type: LogicalType,
}

impl ProductAggregate {
    fn new(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self { input_type })
    }
}

impl AggregateFunctionImpl for ProductAggregate {
    fn kind(&self) -> AggKind {
        AggKind::Udf(AggFnId {
            name: "product".to_string(),
            arg_types: vec![self.input_type.clone()],
        })
    }

    fn input_type(&self) -> &LogicalType {
        &self.input_type
    }

    fn output_type(&self) -> &LogicalType {
        &self.input_type
    }

    fn supports_distinct(&self) -> bool {
        false
    }

    fn create_accumulator(&self, _distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        Ok(Box::new(ProductAccumulator {
            product: DataValue::Int32(1),
            result: DataValue::Null,
            has_values: false,
        }))
    }
}

#[derive(Debug)]
struct ProductAccumulator {
    product: DataValue,
    result: DataValue,
    has_values: bool,
}

impl Accumulator for ProductAccumulator {
    fn update_value(&mut self, value: &DataValue) -> Result<(), DatabaseError> {
        if !value.is_null() {
            self.has_values = true;
            self.product = match (&self.product, value) {
                (DataValue::Int32(a), DataValue::Int32(b)) => DataValue::Int32(a * b),
                (DataValue::Int64(a), DataValue::Int64(b)) => DataValue::Int64(a * b),
                _ => {
                    return Err(DatabaseError::UnsupportedStmt(
                        "Type mismatch in product".to_string(),
                    ))
                }
            };
        }
        Ok(())
    }

    fn evaluate(&mut self) -> Result<(), DatabaseError> {
        if !self.has_values {
            self.result = DataValue::Null;
            return Ok(());
        }
        self.result = self.product.clone();
        Ok(())
    }

    fn result(&self) -> &DataValue {
        &self.result
    }

    fn take_result(&mut self) -> DataValue {
        std::mem::replace(&mut self.result, DataValue::Null)
    }

    fn reset(&mut self) {
        self.product = DataValue::Int32(1);
        self.result = DataValue::Null;
        self.has_values = false;
    }
}

impl fmt::Display for ProductAccumulator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ProductAccumulator(product: {:?})", self.product)
    }
}

fn next_tuple_owned(
    iter: &mut impl kite_sql::db::ResultIter,
) -> Result<Option<kite_sql::types::tuple::TupleImpl>, DatabaseError> {
    iter.next_tuple(|_, tuple| tuple.clone())
}

#[test]
fn test_external_udaf_group_by_having_order_by() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let database = DataBaseBuilder::path(temp_dir.path())
        .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
        .build_in_memory()?;

    let mut iter = database.run(
        "SELECT category, product(value) FROM (VALUES ('A', 2), ('A', 3), ('B', 4), ('B', 5), ('C', 1)) AS t(category, value) GROUP BY category HAVING product(value) > 5 ORDER BY category",
    )?;

    let tuple_a = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_a[1], DataValue::Int32(6));

    let tuple_b = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_b[1], DataValue::Int32(20));

    assert!(next_tuple_owned(&mut iter)?.is_none());
    iter.done()?;

    Ok(())
}

#[test]
fn test_external_udaf_mixed_with_builtins() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let database = DataBaseBuilder::path(temp_dir.path())
        .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
        .build_in_memory()?;

    let mut iter = database.run(
        "SELECT category, product(value), sum(value), count(value) FROM (VALUES ('X', 2), ('X', 3), ('Y', 5)) AS t(category, value) GROUP BY category ORDER BY category",
    )?;

    let tuple_x = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_x[1], DataValue::Int32(6));
    assert_eq!(tuple_x[2], DataValue::Int32(5));
    assert_eq!(tuple_x[3], DataValue::Int32(2));

    let tuple_y = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_y[1], DataValue::Int32(5));
    assert_eq!(tuple_y[2], DataValue::Int32(5));
    assert_eq!(tuple_y[3], DataValue::Int32(1));

    iter.done()?;
    Ok(())
}

#[test]
fn test_external_udaf_null_handling() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let database = DataBaseBuilder::path(temp_dir.path())
        .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
        .build_in_memory()?;

    let mut iter = database.run(
        "SELECT category, product(value) FROM (VALUES ('A', 2), ('A', NULL), ('A', 3), ('B', NULL)) AS t(category, value) GROUP BY category ORDER BY category",
    )?;
    let tuple_a = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_a[1], DataValue::Int32(6));

    let tuple_b = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_b[1], DataValue::Null);

    iter.done()?;

    Ok(())
}
