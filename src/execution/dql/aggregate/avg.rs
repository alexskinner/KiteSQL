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
use crate::execution::dql::aggregate::sum::{text_as_number, SumAccumulator};
use crate::execution::dql::aggregate::Accumulator;
use crate::execution::hash::FxHashSet;
use crate::expression::BinaryOperator;
use crate::types::evaluator::binary_create;
use crate::types::value::DataValue;
use crate::types::LogicalType;
use rust_decimal::{Decimal, RoundingStrategy};
use std::borrow::Cow;
use std::mem;

pub struct AvgAccumulator {
    distinct_values: Option<FxHashSet<DataValue>>,
    inner: Option<SumAccumulator>,
    /// `Some` when AVG is typed DECIMAL: the exact sum, and the result scale (if fixed).
    exact: Option<(Decimal, Option<u8>)>,
    count: usize,
    result: DataValue,
}

impl AvgAccumulator {
    /// An AVG typed `ty`: DECIMAL(p, s) sums exactly and rounds the quotient half away from
    /// zero to scale s; any other type averages as DOUBLE.
    pub fn new(distinct: bool, ty: &LogicalType) -> Self {
        let exact = match ty {
            LogicalType::Decimal(_, scale) => Some((Decimal::ZERO, *scale)),
            _ => None,
        };
        Self {
            distinct_values: distinct.then(FxHashSet::default),
            inner: None,
            exact,
            count: 0,
            result: DataValue::Null,
        }
    }
}

impl Accumulator for AvgAccumulator {
    fn update_value(&mut self, value: &DataValue) -> Result<(), DatabaseError> {
        if !value.is_null() {
            if let Some(values) = &mut self.distinct_values {
                // SAFETY: an executor resets its accumulators before it runs again.
                if !values.insert(unsafe { value.clone_in_txn() }) {
                    return Ok(());
                }
            }
            if let Some((sum, _)) = &mut self.exact {
                // Booleans average as 0 and 1.
                let number = match value {
                    DataValue::Boolean(b) => DataValue::Int32(i32::from(*b)),
                    value => value.clone(),
                };
                let DataValue::Decimal(number) = number.cast(&LogicalType::Decimal(None, None))?
                else {
                    return Err(DatabaseError::InvalidValue(format!(
                        "AVG cannot average {value} as DECIMAL"
                    )));
                };
                *sum = sum.checked_add(number).ok_or(DatabaseError::OverFlow)?;
                self.count += 1;
                self.result = DataValue::Null;
                return Ok(());
            }
            let value = text_as_number(value);
            let acc = if let Some(ref mut inner) = self.inner {
                inner
            } else {
                self.inner
                    .get_or_insert(SumAccumulator::new(Cow::Owned(value.logical_type()))?)
            };
            acc.update_value(&value)?;
            self.count += 1;
            self.result = DataValue::Null;
        }

        Ok(())
    }

    fn evaluate(&mut self) -> Result<(), DatabaseError> {
        if !self.result.is_null() {
            return Ok(());
        }
        if let Some((sum, scale)) = &self.exact {
            if self.count == 0 {
                return Ok(());
            }
            let mut avg = sum
                .checked_div(Decimal::from(self.count as u64))
                .ok_or(DatabaseError::OverFlow)?;
            if let Some(scale) = scale {
                avg = avg.round_dp_with_strategy(
                    u32::from(*scale),
                    RoundingStrategy::MidpointAwayFromZero,
                );
                avg.rescale(u32::from(*scale));
            }
            self.result = DataValue::Decimal(avg);
            return Ok(());
        }
        let Some(acc) = &self.inner else {
            return Ok(());
        };
        if self.count == 0 {
            return Ok(());
        }
        // AVG returns Double even when ordinary integer division truncates.
        let value = acc.result().clone().cast(&LogicalType::Double)?;
        let quantity = DataValue::Float64((self.count as f64).into());
        let evaluator = binary_create(Cow::Owned(LogicalType::Double), BinaryOperator::Divide)?;
        self.result = evaluator.binary_eval(Cow::Borrowed(&value), &quantity)?;
        Ok(())
    }

    fn result(&self) -> &DataValue {
        &self.result
    }

    fn take_result(&mut self) -> DataValue {
        mem::replace(&mut self.result, DataValue::Null)
    }

    fn reset(&mut self) {
        if let Some(values) = &mut self.distinct_values {
            values.clear();
        }
        self.inner = None;
        if let Some((sum, _)) = &mut self.exact {
            *sum = Decimal::ZERO;
        }
        self.count = 0;
        self.result = DataValue::Null;
    }
}

// GRCOV_EXCL_START
#[cfg(test)]
#[maybe_async]
mod tests {
    use super::*;

    #[test]
    fn invalidate_cached_result_after_update() -> Result<(), DatabaseError> {
        let mut accumulator = AvgAccumulator::new(false, &LogicalType::Double);
        accumulator.update_value(&DataValue::Int32(2))?;
        accumulator.evaluate()?;
        let first = accumulator.result().clone();

        accumulator.update_value(&DataValue::Int32(4))?;
        accumulator.evaluate()?;
        let second = accumulator.result().clone();

        assert_ne!(first, second);
        accumulator.evaluate()?;
        assert_eq!(&second, accumulator.result());
        assert_eq!(accumulator.take_result(), second);
        Ok(())
    }

    #[test]
    fn distinct_values_ignore_nulls_and_update_cached_result() -> Result<(), DatabaseError> {
        let mut accumulator = AvgAccumulator::new(true, &LogicalType::Double);
        accumulator.update_value(&DataValue::Null)?;
        accumulator.evaluate()?;
        assert_eq!(accumulator.result(), &DataValue::Null);

        for value in [1, 0, 2, 2] {
            accumulator.update_value(&DataValue::Int32(value))?;
        }
        accumulator.evaluate()?;
        assert_eq!(accumulator.result(), &DataValue::Float64(1.0.into()));

        accumulator.update_value(&DataValue::Int32(2))?;
        accumulator.update_value(&DataValue::Null)?;
        accumulator.evaluate()?;
        assert_eq!(accumulator.result(), &DataValue::Float64(1.0.into()));

        accumulator.update_value(&DataValue::Int32(3))?;
        accumulator.evaluate()?;
        assert_eq!(accumulator.take_result(), DataValue::Float64(1.5.into()));
        Ok(())
    }

    #[test]
    fn decimal_typed_avg_is_exact_and_rounded_to_its_scale() -> Result<(), DatabaseError> {
        let decimal = |s: &str| DataValue::Decimal(s.parse().unwrap());
        let mut accumulator = AvgAccumulator::new(false, &LogicalType::Decimal(None, Some(4)));
        for value in [1, 2, 2] {
            accumulator.update_value(&DataValue::Int32(value))?;
        }
        accumulator.update_value(&DataValue::Null)?;
        accumulator.evaluate()?;
        // 5/3 rounds half away from zero at the result scale, and is padded to it.
        assert_eq!(accumulator.result().to_string(), "1.6667");
        assert_eq!(accumulator.take_result(), decimal("1.6667"));

        accumulator.reset();
        accumulator.evaluate()?;
        assert_eq!(accumulator.result(), &DataValue::Null);
        for value in [-1, -2] {
            accumulator.update_value(&DataValue::Int64(value))?;
        }
        accumulator.evaluate()?;
        assert_eq!(accumulator.result().to_string(), "-1.5000");

        let mut scale0 = AvgAccumulator::new(false, &LogicalType::Decimal(None, Some(0)));
        for value in ["-1.5", "-2.5"] {
            scale0.update_value(&decimal(value))?;
        }
        scale0.evaluate()?;
        assert_eq!(scale0.result().to_string(), "-2");

        let mut distinct = AvgAccumulator::new(true, &LogicalType::Decimal(None, Some(6)));
        for value in ["1.25", "1.25", "2.50", "0.10"] {
            distinct.update_value(&decimal(value))?;
        }
        distinct.evaluate()?;
        assert_eq!(distinct.result().to_string(), "1.283333");

        let mut unscaled = AvgAccumulator::new(false, &LogicalType::Decimal(None, None));
        unscaled.update_value(&DataValue::Int32(1))?;
        unscaled.update_value(&DataValue::Int32(2))?;
        unscaled.evaluate()?;
        assert_eq!(unscaled.result(), &decimal("1.5"));
        Ok(())
    }

    #[test]
    fn empty_and_unsigned_results() -> Result<(), DatabaseError> {
        let mut empty = AvgAccumulator::new(false, &LogicalType::Double);
        empty.evaluate()?;
        assert_eq!(empty.result(), &DataValue::Null);
        assert_eq!(empty.take_result(), DataValue::Null);

        let mut accumulator = AvgAccumulator::new(false, &LogicalType::Double);
        accumulator.update_value(&DataValue::UInt32(2))?;
        accumulator.update_value(&DataValue::UInt32(4))?;
        accumulator.evaluate()?;
        assert_eq!(accumulator.take_result(), DataValue::Float64(3.0.into()));
        Ok(())
    }
}
// GRCOV_EXCL_STOP
