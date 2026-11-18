// Tests for AggregateFunctionImpl trait table

#[cfg(test)]
mod tests {
    use crate::errors::DatabaseError;
    use crate::expression::agg::AggKind;
    use crate::expression::agg_function::AggregateFunctionImpl;
    use crate::function::builtin_aggregates::*;
    use crate::types::LogicalType;
    use crate::types::value::DataValue;

    #[test]
    fn test_sum_aggregate_trait() -> Result<(), DatabaseError> {
        let sum_agg = SumAggregate::new(LogicalType::Integer);
        
        assert_eq!(sum_agg.kind(), AggKind::Sum);
        assert_eq!(sum_agg.input_type(), &LogicalType::Integer);
        assert_eq!(sum_agg.output_type(), &LogicalType::Integer);
        assert!(sum_agg.supports_distinct());
        
        // Test creating accumulator
        let mut acc = sum_agg.create_accumulator(false)?;
        acc.update_value(&DataValue::Int32(10))?;
        acc.update_value(&DataValue::Int32(20))?;
        acc.evaluate()?;
        
        match acc.result() {
            DataValue::Int32(30) => {},
            other => panic!("Expected Int32(30), got {:?}", other),
        }
        
        Ok(())
    }

    #[test]
    fn test_count_aggregate_trait() -> Result<(), DatabaseError> {
        let count_agg = CountAggregate::new(LogicalType::Integer);
        
        assert_eq!(count_agg.kind(), AggKind::Count);
        assert_eq!(count_agg.output_type(), &LogicalType::Bigint);
        assert!(count_agg.supports_distinct());
        
        let mut acc = count_agg.create_accumulator(false)?;
        acc.update_value(&DataValue::Int32(1))?;
        acc.update_value(&DataValue::Int32(2))?;
        acc.update_value(&DataValue::Null)?; // Should not count
        acc.evaluate()?;
        
        match acc.result() {
            DataValue::Int32(2) => {},
            other => panic!("Expected Int32(2), got {:?}", other),
        }
        
        Ok(())
    }

    #[test]
    fn test_min_max_aggregate_trait() -> Result<(), DatabaseError> {
        let min_agg = MinMaxAggregate::new_min(LogicalType::Integer);
        let max_agg = MinMaxAggregate::new_max(LogicalType::Integer);
        
        assert_eq!(min_agg.kind(), AggKind::Min);
        assert_eq!(max_agg.kind(), AggKind::Max);
        assert!(!min_agg.supports_distinct());
        
        let mut min_acc = min_agg.create_accumulator(false)?;
        min_acc.update_value(&DataValue::Int32(30))?;
        min_acc.update_value(&DataValue::Int32(10))?;
        min_acc.update_value(&DataValue::Int32(20))?;
        min_acc.evaluate()?;
        
        match min_acc.result() {
            DataValue::Int32(10) => {},
            other => panic!("Expected Int32(10), got {:?}", other),
        }
        
        Ok(())
    }

    #[test]
    fn test_avg_aggregate_trait() -> Result<(), DatabaseError> {
        let avg_agg = AvgAggregate::new(LogicalType::Integer);
        
        assert_eq!(avg_agg.kind(), AggKind::Avg);
        assert_eq!(avg_agg.output_type(), &LogicalType::Double);
        assert!(avg_agg.supports_distinct());
        
        let mut acc = avg_agg.create_accumulator(false)?;
        acc.update_value(&DataValue::Int32(10))?;
        acc.update_value(&DataValue::Int32(20))?;
        acc.update_value(&DataValue::Int32(30))?;
        acc.evaluate()?;
        
        match acc.result() {
            DataValue::Float64(v) if (*v - 20.0).abs() < 0.001 => {},
            other => panic!("Expected Float64(20.0), got {:?}", other),
        }
        
        Ok(())
    }

    #[test]
    fn test_group_concat_aggregate_trait() -> Result<(), DatabaseError> {
        use crate::types::value::Utf8Type;
        use crate::types::CharLengthUnits;
        
        let gc_agg = GroupConcatAggregate::new(LogicalType::Varchar(None, CharLengthUnits::Characters));
        
        assert_eq!(gc_agg.kind(), AggKind::GroupConcat);
        assert!(gc_agg.supports_distinct());
        
        let mut acc = gc_agg.create_accumulator(false)?;
        acc.update_value(&DataValue::Utf8 {
            value: "foo".to_string().into(),
            ty: Utf8Type::Variable(None),
            unit: CharLengthUnits::Characters,
        })?;
        acc.update_value(&DataValue::Utf8 {
            value: "bar".to_string().into(),
            ty: Utf8Type::Variable(None),
            unit: CharLengthUnits::Characters,
        })?;
        acc.evaluate()?;
        
        match acc.result() {
            DataValue::Utf8 { value, .. } if value.as_ref() == "foo,bar" => {},
            other => panic!("Expected 'foo,bar', got {:?}", other),
        }
        
        Ok(())
    }

    /// Property test: Sum equivalence
    /// Verifies that sum(values) == sum of individual adds
    #[test]
    fn test_sum_equivalence_property() -> Result<(), DatabaseError> {
        let sum_agg = SumAggregate::new(LogicalType::Integer);
        
        let values = vec![1, 2, 3, 4, 5, 10, 20, 30];
        let expected_sum: i32 = values.iter().sum();
        
        let mut acc = sum_agg.create_accumulator(false)?;
        for &v in &values {
            acc.update_value(&DataValue::Int32(v))?;
        }
        acc.evaluate()?;
        
        match acc.result() {
            DataValue::Int32(result) if *result == expected_sum => {},
            other => panic!("Sum equivalence failed: expected {}, got {:?}", expected_sum, other),
        }
        
        Ok(())
    }
}
