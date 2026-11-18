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

#![allow(dead_code, unused_variables)]

#[cfg(test)]
mod tests {
    use crate::db::DataBaseBuilder;
    use crate::errors::DatabaseError;
    use crate::execution::dql::aggregate::Accumulator;
    use crate::expression::agg::{AggFnId, AggKind};
    use crate::expression::agg_function::AggregateFunctionImpl;
    use crate::function::builtin_aggregates::*;
    use crate::types::value::DataValue;
    use crate::types::LogicalType;
    use std::fmt;
    use std::sync::Arc;

    #[test]
    fn test_sum_aggregate_trait() -> Result<(), DatabaseError> {
        let _sum_agg = SumAggregate::new(LogicalType::Integer);
        Ok(())
    }

    #[test]
    fn test_count_aggregate_trait() -> Result<(), DatabaseError> {
        let _count_agg = CountAggregate::new(LogicalType::SqlNull);
        Ok(())
    }

    #[test]
    fn test_min_max_aggregate_trait() -> Result<(), DatabaseError> {
        let _min_agg = MinMaxAggregate::new_min(LogicalType::Integer);
        let _max_agg = MinMaxAggregate::new_max(LogicalType::Integer);
        Ok(())
    }

    #[test]
    fn test_avg_aggregate_trait() -> Result<(), DatabaseError> {
        let _avg_agg = AvgAggregate::new(LogicalType::Integer);
        Ok(())
    }

    #[test]
    fn test_group_concat_aggregate_trait() -> Result<(), DatabaseError> {
        use crate::types::CharLengthUnits;
        let _group_concat = GroupConcatAggregate::new(LogicalType::Varchar(None, CharLengthUnits::Characters));
        Ok(())
    }

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

        fn create_accumulator(&self, _distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
            Ok(Box::new(SumTimes10Accumulator {
                sum: DataValue::Int32(0),
                result: DataValue::Null,
            }))
        }
    }

    #[derive(Debug)]
    struct SumTimes10Accumulator {
        sum: DataValue,
        result: DataValue,
    }

    impl Accumulator for SumTimes10Accumulator {
        fn update_value(&mut self, value: &DataValue) -> Result<(), DatabaseError> {
            if !value.is_null() {
                self.sum = match (&self.sum, value) {
                    (DataValue::Int32(a), DataValue::Int32(b)) => DataValue::Int32(a + b),
                    (DataValue::Int64(a), DataValue::Int64(b)) => DataValue::Int64(a + b),
                    _ => return Err(DatabaseError::UnsupportedStmt("Type mismatch in sum_times_10".to_string())),
                };
            }
            Ok(())
        }

        fn evaluate(&mut self) -> Result<(), DatabaseError> {
            self.result = match &self.sum {
                DataValue::Int32(val) => DataValue::Int32(val * 10),
                DataValue::Int64(val) => DataValue::Int64(val * 10),
                _ => return Err(DatabaseError::UnsupportedStmt("Invalid type for sum_times_10".to_string())),
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
        }
    }

    impl fmt::Display for SumTimes10Accumulator {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "SumTimes10Accumulator(sum: {:?})", self.sum)
        }
    }

    #[test]
    fn test_register_and_call_udaf() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path("test")
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()?;

        database.run("create table t1(id int, category varchar, value int)")?;
        database.run("insert into t1 values (1, 'A', 10), (2, 'A', 20), (3, 'B', 15)")?;

        let mut iter = database.run("select sum_times_10(value) from t1")?;
        let tuple = iter.next()?.unwrap();
        assert_eq!(tuple.values[0], DataValue::Int32(450));
        iter.done()?;

        Ok(())
    }

    #[test]
    fn test_udaf_with_group_by() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path("test")
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()?;

        database.run("create table t1(id int, category varchar, value int)")?;
        database.run("insert into t1 values (1, 'A', 10), (2, 'A', 20), (3, 'B', 15), (4, 'B', 25)")?;

        let mut iter = database.run("select category, sum_times_10(value) from t1 group by category order by category")?;
        
        let tuple_a = iter.next()?.unwrap();
        assert_eq!(tuple_a.values[1], DataValue::Int32(300));
        
        let tuple_b = iter.next()?.unwrap();
        assert_eq!(tuple_b.values[1], DataValue::Int32(400));
        
        iter.done()?;
        Ok(())
    }

    #[test]
    fn test_udaf_null_handling() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path("test")
            .register_aggregate_function(Arc::new(SumTimes10Aggregate::new(LogicalType::Integer)))
            .build_in_memory()?;

        database.run("create table t1(id int, value int)")?;
        database.run("insert into t1 values (1, 10), (2, NULL), (3, 20)")?;

        let mut iter = database.run("select sum_times_10(value) from t1")?;
        let tuple = iter.next()?.unwrap();
        assert_eq!(tuple.values[0], DataValue::Int32(300));
        iter.done()?;

        Ok(())
    }
}
