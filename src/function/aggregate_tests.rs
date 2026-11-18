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
    use crate::errors::DatabaseError;
    use crate::expression::agg::AggKind;
    use crate::expression::agg_function::AggregateFunctionImpl;
    use crate::function::builtin_aggregates::*;
    use crate::types::value::DataValue;
    use crate::types::LogicalType;

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
        use crate::types::value::Utf8Type;
        let _group_concat = GroupConcatAggregate::new(LogicalType::Varchar(None, Utf8Type::Variable(None)));
        Ok(())
    }
}
