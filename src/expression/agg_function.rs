// AggregateFunctionImpl trait for pluggable aggregate functions

use crate::errors::DatabaseError;
use crate::execution::dql::aggregate::Accumulator;
use crate::expression::agg::AggKind;
use crate::types::LogicalType;
use std::fmt::Debug;
use std::sync::Arc;

/// High-level trait for aggregate function implementations.
/// This provides metadata and factory methods for creating accumulators.
pub trait AggregateFunctionImpl: Debug + Send + Sync {
    /// Returns the aggregate kind this implementation handles
    fn kind(&self) -> AggKind;

    /// Returns the input type this implementation accepts
    fn input_type(&self) -> &LogicalType;

    /// Returns the output type this implementation produces
    fn output_type(&self) -> &LogicalType;

    /// Whether this aggregate supports DISTINCT
    fn supports_distinct(&self) -> bool;

    /// Create a new accumulator instance for this aggregate
    fn create_accumulator(&self, distinct: bool) -> Result<Box<dyn Accumulator>, DatabaseError>;
}

/// Wrapper for Arc<dyn AggregateFunctionImpl> to impl Clone, PartialEq, etc.
#[derive(Debug, Clone)]
pub struct ArcAggregateFunctionImpl(pub Arc<dyn AggregateFunctionImpl>);

impl std::ops::Deref for ArcAggregateFunctionImpl {
    type Target = dyn AggregateFunctionImpl;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl PartialEq for ArcAggregateFunctionImpl {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ArcAggregateFunctionImpl {}

impl std::hash::Hash for ArcAggregateFunctionImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// A unique identifier for an aggregate function: kind + input type
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AggregateFunctionId {
    pub kind: AggKind,
    pub input_type: LogicalType,
}

impl AggregateFunctionId {
    pub fn new(kind: AggKind, input_type: LogicalType) -> Self {
        Self { kind, input_type }
    }
}
