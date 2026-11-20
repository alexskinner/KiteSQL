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

#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
mod tests {
    use crate::db::DataBaseBuilder;
    use crate::errors::DatabaseError;
    use crate::execution::dql::aggregate::Accumulator;
    use crate::expression::agg::{AggFnId, AggKind};
    use crate::expression::agg_function::AggregateFunctionImpl;
    use crate::types::value::DataValue;
    use crate::types::LogicalType;
    use std::fmt;
    use std::sync::Arc;

    #[derive(Debug)]
    struct SumTimes10Aggregate {
        input_type: LogicalType,
    }

    impl SumTimes10Aggregate {
        fn new(input_type: LogicalType) -> Self {
            Self { input_type }
        }
    }

    impl AggregateFunctionImpl for SumTimes10Aggregate {
        fn kind(&self) -> AggKind {
            AggKind::Udf(AggFnId {
                name: "sum_times_10".to_string(),
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

        fn create_accumulator(
            &self,
            _distinct: bool,
        ) -> Result<Box<dyn Accumulator>, DatabaseError> {
            Ok(Box::new(SumTimes10Accumulator {
                sum: DataValue::Int32(0),
                result: DataValue::Null,
                has_values: false,
            }))
        }
    }

    #[derive(Debug)]
    struct SumTimes10Accumulator {
        sum: DataValue,
        result: DataValue,
        has_values: bool,
    }

    impl Accumulator for SumTimes10Accumulator {
        fn update_value(&mut self, value: &DataValue) -> Result<(), DatabaseError> {
            if !value.is_null() {
                self.has_values = true;
                self.sum = match (&self.sum, value) {
                    (DataValue::Int32(a), DataValue::Int32(b)) => DataValue::Int32(a + b),
                    (DataValue::Int64(a), DataValue::Int64(b)) => DataValue::Int64(a + b),
                    _ => {
                        return Err(DatabaseError::UnsupportedStmt(
                            "Type mismatch in sum_times_10".to_string(),
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
            self.result = match &self.sum {
                DataValue::Int32(val) => DataValue::Int32(val * 10),
                DataValue::Int64(val) => DataValue::Int64(val * 10),
                _ => {
                    return Err(DatabaseError::UnsupportedStmt(
                        "Invalid type for sum_times_10".to_string(),
                    ))
                }
            };
            Ok(())
        }

        fn result(&self) -> &DataValue {
            &self.result
        }

        fn take_result(&mut self) -> DataValue {
            std::mem::replace(&mut self.result, DataValue::Null)
        }

        fn reset(&mut self) {
            self.sum = DataValue::Int32(0);
            self.result = DataValue::Null;
            self.has_values = false;
        }
    }

    impl fmt::Display for SumTimes10Accumulator {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "SumTimes10Accumulator(sum: {:?})", self.sum)
        }
    }

    async fn next_tuple_owned(
        iter: &mut impl crate::db::ResultIter,
    ) -> Result<Option<crate::types::tuple::TupleImpl>, DatabaseError> {
        iter.next_tuple(|_, tuple| tuple.clone()).await
    }

    // Simplified UDAF test using VALUES clause instead of tables
    #[test]
    async fn test_register_and_call_udaf() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        // Test aggregate on VALUES clause
        let mut iter = database
            .run("select sum_times_10(value) from (values (10), (20), (15)) as t(value)")
            .await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(450));
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_with_group_by() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        // Test aggregate with GROUP BY on VALUES clause
        let mut iter = database.run("select category, sum_times_10(value) from (values ('A', 10), ('A', 20), ('B', 15), ('B', 25)) as t(category, value) group by category order by category").await?;

        let tuple_a = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple_a[1], DataValue::Int32(300));

        let tuple_b = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple_b[1], DataValue::Int32(400));

        iter.done().await?;
        Ok(())
    }

    #[test]
    async fn test_udaf_null_handling() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        // Test NULL handling in aggregate
        let mut iter = database
            .run("select sum_times_10(value) from (values (10), (NULL), (20)) as t(value)")
            .await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(300));
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_with_builtin_aggregates() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        let mut iter = database.run(
            "select sum_times_10(value), sum(value), count(value) from (values (10), (20), (15)) as t(value)"
        ).await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(450));
        assert_eq!(tuple[1], DataValue::Int32(45));
        assert_eq!(tuple[2], DataValue::Int32(3));
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_with_having() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        let mut iter = database.run(
            "select category, sum_times_10(value) from (values ('A', 10), ('A', 20), ('B', 5), ('B', 5)) as t(category, value) group by category having sum_times_10(value) > 200 order by category"
        ).await?;

        let tuple_a = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple_a[1], DataValue::Int32(300));

        assert!(next_tuple_owned(&mut iter).await?.is_none());
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_empty_result() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        let mut iter = database
            .run("select sum_times_10(value) from (values (1)) as t(value) where value > 100")
            .await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Null);
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_empty_table() -> Result<(), DatabaseError> {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        database
            .ddl("CREATE TABLE empty_table_test (id INT PRIMARY KEY, value INT)")
            .await?;

        let mut iter = database
            .run("select sum_times_10(value) from empty_table_test")
            .await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Null);
        iter.done().await?;

        Ok(())
    }

    #[test]
    async fn test_udaf_all_null_group() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()
            .await?;

        let mut iter = database.run(
            "select category, sum_times_10(value) from (values ('A', 10), ('B', NULL)) as t(category, value) group by category order by category"
        ).await?;

        let tuple_a = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple_a[1], DataValue::Int32(100));

        let tuple_b = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple_b[1], DataValue::Null);

        iter.done().await?;
        Ok(())
    }

    #[test]
    async fn test_udaf_name_clash_with_builtin() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        #[derive(Debug)]
        struct SumOverride {
            input_type: LogicalType,
        }

        impl AggregateFunctionImpl for SumOverride {
            fn kind(&self) -> AggKind {
                AggKind::Udf(AggFnId {
                    name: "sum".to_string(),
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

            fn create_accumulator(
                &self,
                _distinct: bool,
            ) -> Result<Box<dyn Accumulator>, DatabaseError> {
                Ok(Box::new(SumTimes10Accumulator {
                    sum: DataValue::Int32(0),
                    result: DataValue::Null,
                    has_values: false,
                }))
            }
        }

        let database = DataBaseBuilder::path(temp_dir.path())
            .register_aggregate_function(Arc::new(SumOverride {
                input_type: LogicalType::Integer,
            }))
            .build_in_memory()
            .await?;

        let mut iter = database
            .run("select sum(value) from (values (10), (20)) as t(value)")
            .await?;
        let tuple = next_tuple_owned(&mut iter).await?.unwrap();
        assert_eq!(tuple[0], DataValue::Int32(300));
        iter.done().await?;

        Ok(())
    }
}
