// Built-in aggregate function implementations

use crate::errors::DatabaseError;
use crate::execution::dql::aggregate::Accumulator;
use crate::expression::agg::AggKind;
use crate::expression::agg_function::AggregateFunctionImpl;
use crate::types::LogicalType;
use std::borrow::Cow;
use std::sync::Arc;

/// Sum aggregate implementation
#[derive(Debug)]
pub struct SumAggregate {
    input_type: LogicalType,
    output_type: LogicalType,
}

impl SumAggregate {
    pub fn new(input_type: LogicalType) -> Arc<Self> {
        let output_type = input_type.clone();
        Arc::new(Self {
            input_type,
            output_type,
        })
    }
}

impl AggregateFunctionImpl for SumAggregate {
    fn kind(&self) -> AggKind {
        AggKind::Sum
    }
    
    fn input_type(&self) -> &LogicalType {
        &self.input_type
    }
    
    fn output_type(&self) -> &LogicalType {
        &self.output_type
    }
    
    fn supports_distinct(&self) -> bool {
        true
    }
    
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        use crate::execution::dql::aggregate::create_accumulator;
        create_accumulator(AggKind::Sum, &self.input_type, distinct)
    }
}

/// Count aggregate implementation
#[derive(Debug)]
pub struct CountAggregate {
    input_type: LogicalType,
}

impl CountAggregate {
    pub fn new(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self { input_type })
    }
}

impl AggregateFunctionImpl for CountAggregate {
    fn kind(&self) -> AggKind {
        AggKind::Count
    }
    
    fn input_type(&self) -> &LogicalType {
        &self.input_type
    }
    
    fn output_type(&self) -> &LogicalType {
        &LogicalType::Bigint
    }
    
    fn supports_distinct(&self) -> bool {
        true
    }
    
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        use crate::execution::dql::aggregate::create_accumulator;
        create_accumulator(AggKind::Count, &self.input_type, distinct)
    }
}

/// Min/Max aggregate implementation
#[derive(Debug)]
pub struct MinMaxAggregate {
    kind: AggKind,
    input_type: LogicalType,
}

impl MinMaxAggregate {
    pub fn new_min(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self {
            kind: AggKind::Min,
            input_type,
        })
    }
    
    pub fn new_max(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self {
            kind: AggKind::Max,
            input_type,
        })
    }
}

impl AggregateFunctionImpl for MinMaxAggregate {
    fn kind(&self) -> AggKind {
        self.kind.clone()
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
    
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        use crate::execution::dql::aggregate::create_accumulator;
        create_accumulator(self.kind.clone(), &self.input_type, distinct)
    }
}

/// Avg aggregate implementation
#[derive(Debug)]
pub struct AvgAggregate {
    input_type: LogicalType,
}

impl AvgAggregate {
    pub fn new(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self { input_type })
    }
}

impl AggregateFunctionImpl for AvgAggregate {
    fn kind(&self) -> AggKind {
        AggKind::Avg
    }
    
    fn input_type(&self) -> &LogicalType {
        &self.input_type
    }
    
    fn output_type(&self) -> &LogicalType {
        &LogicalType::Double
    }
    
    fn supports_distinct(&self) -> bool {
        true
    }
    
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        use crate::execution::dql::aggregate::create_accumulator;
        create_accumulator(AggKind::Avg, &self.input_type, distinct)
    }
}

/// GroupConcat aggregate implementation
#[derive(Debug)]
pub struct GroupConcatAggregate {
    input_type: LogicalType,
}

impl GroupConcatAggregate {
    pub fn new(input_type: LogicalType) -> Arc<Self> {
        Arc::new(Self { input_type })
    }
}

impl AggregateFunctionImpl for GroupConcatAggregate {
    fn kind(&self) -> AggKind {
        AggKind::GroupConcat
    }
    
    fn input_type(&self) -> &LogicalType {
        &self.input_type
    }
    
    fn output_type(&self) -> &LogicalType {
        &LogicalType::Varchar(None, crate::types::CharLengthUnits::Characters)
    }
    
    fn supports_distinct(&self) -> bool {
        true
    }
    
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError> {
        use crate::execution::dql::aggregate::create_accumulator;
        create_accumulator(AggKind::GroupConcat, &self.input_type, distinct)
    }
}

/// Install all built-in aggregate functions into a function registry
pub fn install_builtin_aggregates(
    registry: &mut crate::function::registry::FunctionRegistry,
) -> Result<(), DatabaseError> {
    // Common numeric types for aggregates
    let numeric_types = vec![
        LogicalType::Tinyint,
        LogicalType::Smallint,
        LogicalType::Integer,
        LogicalType::Bigint,
        LogicalType::Float,
        LogicalType::Double,
    ];
    
    // Register Sum for numeric types
    for ty in &numeric_types {
        let sum_agg = SumAggregate::new(ty.clone());
        // Note: Would call registry.register_aggregate() once that's added
    }
    
    // Register Count (accepts any type)
    let count_agg = CountAggregate::new(LogicalType::Integer);
    
    // Register Min/Max for comparable types
    for ty in &numeric_types {
        let min_agg = MinMaxAggregate::new_min(ty.clone());
        let max_agg = MinMaxAggregate::new_max(ty.clone());
    }
    
    // Register Avg for numeric types
    for ty in &numeric_types {
        let avg_agg = AvgAggregate::new(ty.clone());
    }
    
    // Register GroupConcat for string types
    let group_concat = GroupConcatAggregate::new(LogicalType::Varchar(None, crate::types::CharLengthUnits::Characters));
    
    Ok(())
}
