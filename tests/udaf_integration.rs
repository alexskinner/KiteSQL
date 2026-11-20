// Integration test to verify AggregateFunctionImpl and Accumulator can be implemented from outside kite_sql

// The tests use `tempfile`, a native-only dev-dependency.
#![cfg(all(not(target_arch = "wasm32"), not(feature = "async")))]

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

#[cfg(feature = "orm")]
#[test]
fn test_external_udaf_in_view() -> Result<(), DatabaseError> {
    use kite_sql::Model;

    #[derive(Debug, Clone, Model)]
    #[model(table = "product_data")]
    struct ProductData {
        #[model(primary_key)]
        id: i32,
        category: String,
        multiplier: i32,
    }

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
        .build_in_memory()?;

    database.create_table_if_not_exists::<ProductData>()?;
    database.insert(&ProductData {
        id: 1,
        category: "X".to_string(),
        multiplier: 2,
    })?;
    database.insert(&ProductData {
        id: 2,
        category: "X".to_string(),
        multiplier: 3,
    })?;
    database.insert(&ProductData {
        id: 3,
        category: "Y".to_string(),
        multiplier: 5,
    })?;

    database
        .ddl("CREATE VIEW product_view AS SELECT category, product(multiplier) as total FROM product_data GROUP BY category")?;

    let mut iter = database.run("SELECT category, total FROM product_view ORDER BY category")?;

    let tuple_x = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_x[1], DataValue::Int32(6));

    let tuple_y = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple_y[1], DataValue::Int32(5));

    iter.done()?;
    Ok(())
}

#[cfg(feature = "lmdb")]
#[test]
fn test_external_udaf_lmdb_persist_with_view() -> Result<(), DatabaseError> {
    use kite_sql::db::CatalogKind;

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    {
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
            .build_lmdb()?;

        database.ddl("CREATE TABLE test_data (id INT PRIMARY KEY, val INT)")?;
        database
            .run("INSERT INTO test_data (id, val) VALUES (1, 3), (2, 4)")?
            .done()?;

        database
            .ddl("CREATE VIEW product_summary AS SELECT product(val) as total FROM test_data")?;

        let mut iter = database.run("SELECT product(val) FROM test_data")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(12));
        iter.done()?;

        let mut iter2 = database.run("SELECT total FROM product_summary")?;
        let tuple2 = next_tuple_owned(&mut iter2)?.unwrap();
        assert_eq!(tuple2[0], DataValue::Int32(12));
        iter2.done()?;
    }

    {
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
            .build_lmdb()?;

        database.load(CatalogKind::Table("test_data".into()))?;
        database.load(CatalogKind::View("product_summary".into()))?;

        let mut iter = database.run("SELECT product(val) FROM test_data")?;
        let tuple = next_tuple_owned(&mut iter)?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(12));
        iter.done()?;

        let mut iter2 = database.run("SELECT total FROM product_summary")?;
        let tuple2 = next_tuple_owned(&mut iter2)?.unwrap();
        assert_eq!(tuple2[0], DataValue::Int32(12));
        iter2.done()?;
    }

    Ok(())
}

#[cfg(feature = "orm")]
#[test]
fn test_external_udaf_empty_table() -> Result<(), DatabaseError> {
    use kite_sql::Model;

    #[derive(Debug, Clone, Model)]
    #[model(table = "empty_test")]
    struct EmptyTest {
        #[model(primary_key)]
        id: i32,
        value: i32,
    }

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .register_aggregate_function(ProductAggregate::new(LogicalType::Integer))
        .build_in_memory()?;

    database.create_table_if_not_exists::<EmptyTest>()?;

    let mut iter = database.run("SELECT product(value) FROM empty_test")?;
    let tuple = next_tuple_owned(&mut iter)?.unwrap();
    assert_eq!(tuple[0], DataValue::Null);
    iter.done()?;

    Ok(())
}
