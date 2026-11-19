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
use crate::execution::dql::aggregate::Accumulator;
use crate::execution::hash::FxHashSet;
use crate::expression::agg::{GroupConcatOptions, GroupConcatOrder};
use crate::types::value::{DataValue, Utf8Type};
use crate::types::CharLengthUnits;
use std::cmp::Ordering;
use std::fmt::Write;
use std::mem;

/// `group_concat()`: non-NULL values joined with a separator (`,` by default), in input order or
/// sorted by the ORDER BY keys; NULL when there are none. With several value arguments each row's
/// values are concatenated, and a row with any NULL value is skipped (MySQL).
pub struct GroupConcatAccumulator {
    distinct_values: Option<FxHashSet<DataValue>>,
    separator: String,
    order_by: Vec<GroupConcatOrder>,
    /// With an ORDER BY: each value's text and its sort keys, in input order.
    rows: Vec<(String, Vec<DataValue>)>,
    result: DataValue,
}

impl GroupConcatAccumulator {
    pub fn new(distinct: bool) -> Self {
        Self::with_options(
            distinct,
            &GroupConcatOptions {
                separator: GroupConcatOptions::DEFAULT_SEPARATOR.to_string(),
                order_by: Vec::new(),
            },
        )
    }

    pub fn with_options(distinct: bool, options: &GroupConcatOptions) -> Self {
        Self {
            distinct_values: distinct.then(FxHashSet::default),
            separator: options.separator.clone(),
            order_by: options.order_by.clone(),
            rows: Vec::new(),
            result: DataValue::Null,
        }
    }

    fn utf8(text: String) -> DataValue {
        DataValue::Utf8 {
            value: text.into(),
            ty: Utf8Type::Variable(None),
            unit: CharLengthUnits::Characters,
        }
    }

    /// Orders two rows' sort keys as a query's ORDER BY does.
    fn compare_keys(
        order_by: &[GroupConcatOrder],
        left: &[DataValue],
        right: &[DataValue],
    ) -> Ordering {
        for ((left, right), order) in left.iter().zip(right).zip(order_by) {
            let ordering = match (left.is_null(), right.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if order.nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if order.nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => {
                    let ordering = left.partial_cmp(right).unwrap_or(Ordering::Equal);
                    if order.asc {
                        ordering
                    } else {
                        ordering.reverse()
                    }
                }
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// Whether `value` is new, for DISTINCT; always true otherwise.
    fn is_new(&mut self, value: &DataValue) -> bool {
        let Some(values) = &mut self.distinct_values else {
            return true;
        };
        if values.contains(value) {
            return false;
        }
        let mut value = value.clone();
        value.materialize();
        values.insert(value)
    }
}

impl GroupConcatAccumulator {
    /// One row's text: its single value, or its several values concatenated.
    fn row_text(values: &[DataValue]) -> String {
        let mut text = String::new();
        for value in values {
            let _ = write!(text, "{value}");
        }
        text
    }

    fn append(&mut self, row: String) {
        match &mut self.result {
            DataValue::Utf8 { value: text, .. } => {
                let text = text.to_mut();
                text.push_str(&self.separator);
                text.push_str(&row);
            }
            result => *result = Self::utf8(row),
        }
    }
}

impl Accumulator for GroupConcatAccumulator {
    fn update_value(&mut self, value: &DataValue) -> Result<(), DatabaseError> {
        // Without an ORDER BY, a tuple carries several values; with one, it carries the values
        // followed by one key per ORDER BY entry.
        let (values, keys) = match value {
            DataValue::Tuple(values) => {
                let Some(split) = values
                    .len()
                    .checked_sub(self.order_by.len())
                    .filter(|split| *split > 0)
                else {
                    return Err(DatabaseError::InvalidValue(format!(
                        "group_concat() expects its values and {} sort key(s), got {value}",
                        self.order_by.len()
                    )));
                };
                values.split_at(split)
            }
            value if self.order_by.is_empty() => (std::slice::from_ref(value), &[][..]),
            value => {
                return Err(DatabaseError::InvalidValue(format!(
                    "group_concat() with ORDER BY expects its value and sort keys, got {value}"
                )))
            }
        };
        if values.iter().any(DataValue::is_null) {
            return Ok(());
        }
        let is_new = match values {
            [value] => self.is_new(value),
            values => self.is_new(&DataValue::Tuple(values.to_vec())),
        };
        if !is_new {
            return Ok(());
        }
        if self.order_by.is_empty() {
            let row = Self::row_text(values);
            self.append(row);
            return Ok(());
        }
        let keys = keys
            .iter()
            .map(|key| {
                let mut key = key.clone();
                key.materialize();
                key
            })
            .collect();
        self.rows.push((Self::row_text(values), keys));
        Ok(())
    }

    fn evaluate(&mut self) -> Result<(), DatabaseError> {
        if self.order_by.is_empty() || self.rows.is_empty() {
            return Ok(());
        }
        // A stable sort keeps rows with equal keys in input order.
        let order_by = &self.order_by;
        self.rows
            .sort_by(|(_, left), (_, right)| Self::compare_keys(order_by, left, right));
        let mut text = String::new();
        for (i, (value, _)) in self.rows.iter().enumerate() {
            if i > 0 {
                text.push_str(&self.separator);
            }
            text.push_str(value);
        }
        self.result = Self::utf8(text);
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
        self.rows.clear();
        self.result = DataValue::Null;
    }
}

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::GroupConcatAccumulator;
    use crate::errors::DatabaseError;
    use crate::execution::dql::aggregate::Accumulator;
    use crate::expression::agg::{GroupConcatOptions, GroupConcatOrder};
    use crate::types::value::DataValue;

    fn text(value: &str) -> DataValue {
        DataValue::Utf8 {
            value: value.to_string().into(),
            ty: crate::types::value::Utf8Type::Variable(None),
            unit: crate::types::CharLengthUnits::Characters,
        }
    }

    fn result(accumulator: &mut GroupConcatAccumulator) -> Result<String, DatabaseError> {
        accumulator.evaluate()?;
        Ok(accumulator.result().to_string())
    }

    fn options(separator: &str, order_by: &[(bool, bool)]) -> GroupConcatOptions {
        GroupConcatOptions {
            separator: separator.to_string(),
            order_by: order_by
                .iter()
                .map(|&(asc, nulls_first)| GroupConcatOrder { asc, nulls_first })
                .collect(),
        }
    }

    #[test]
    fn test_group_concat_separator_and_distinct() -> Result<(), DatabaseError> {
        let mut accumulator = GroupConcatAccumulator::with_options(true, &options("|", &[]));
        for value in [text("x"), DataValue::Null, text("y"), text("x"), text("z")] {
            accumulator.update_value(&value)?;
        }
        assert_eq!(result(&mut accumulator)?, "x|y|z");

        accumulator.reset();
        assert!(accumulator.result().is_null());
        accumulator.update_value(&text("x"))?;
        assert_eq!(result(&mut accumulator)?, "x");
        Ok(())
    }

    #[test]
    fn test_group_concat_order_by() -> Result<(), DatabaseError> {
        let row = |value: DataValue, key: DataValue| DataValue::Tuple(vec![value, key]);
        let rows = [
            row(text("c"), DataValue::Int32(3)),
            row(text("a"), DataValue::Int32(1)),
            row(DataValue::Null, DataValue::Int32(0)),
            row(text("n"), DataValue::Null),
            row(text("b"), DataValue::Int32(1)),
        ];
        for (order, distinct, expected) in [
            ((true, false), false, "a-b-c-n"),
            ((false, false), false, "c-a-b-n"),
            ((true, true), false, "n-a-b-c"),
            ((false, true), false, "n-c-a-b"),
        ] {
            let mut accumulator =
                GroupConcatAccumulator::with_options(distinct, &options("-", &[order]));
            for value in &rows {
                accumulator.update_value(value)?;
            }
            assert_eq!(result(&mut accumulator)?, expected, "{order:?}");
        }

        // DISTINCT applies to the value; the first row of a duplicate keeps its key.
        let mut accumulator =
            GroupConcatAccumulator::with_options(true, &options(",", &[(true, false)]));
        for value in [
            row(text("x"), DataValue::Int32(2)),
            row(text("y"), DataValue::Int32(1)),
            row(text("x"), DataValue::Int32(0)),
        ] {
            accumulator.update_value(&value)?;
        }
        assert_eq!(result(&mut accumulator)?, "y,x");

        // A window frame evaluates, then grows, then evaluates again.
        let mut accumulator =
            GroupConcatAccumulator::with_options(false, &options(",", &[(false, false)]));
        accumulator.update_value(&row(text("a"), DataValue::Int32(1)))?;
        assert_eq!(result(&mut accumulator)?, "a");
        accumulator.update_value(&row(text("b"), DataValue::Int32(2)))?;
        assert_eq!(result(&mut accumulator)?, "b,a");

        assert!(accumulator.update_value(&text("not a tuple")).is_err());
        Ok(())
    }

    #[test]
    fn test_group_concat_several_values() -> Result<(), DatabaseError> {
        let row = |values: Vec<DataValue>| DataValue::Tuple(values);
        // Each row's values are concatenated; a row with any NULL value is skipped.
        let mut accumulator = GroupConcatAccumulator::with_options(false, &options("|", &[]));
        for value in [
            row(vec![DataValue::Int32(1), text("-"), text("a")]),
            row(vec![DataValue::Int32(2), text("-"), DataValue::Null]),
            row(vec![DataValue::Int32(3), text("-"), text("c")]),
        ] {
            accumulator.update_value(&value)?;
        }
        assert_eq!(result(&mut accumulator)?, "1-a|3-c");

        // DISTINCT compares the values as a tuple, not their concatenation.
        let mut accumulator = GroupConcatAccumulator::with_options(true, &options(",", &[]));
        for value in [
            row(vec![text("a"), text("bc")]),
            row(vec![text("ab"), text("c")]),
            row(vec![text("a"), text("bc")]),
        ] {
            accumulator.update_value(&value)?;
        }
        assert_eq!(result(&mut accumulator)?, "abc,abc");

        // With an ORDER BY the trailing entries are the sort keys.
        let mut accumulator =
            GroupConcatAccumulator::with_options(false, &options(" ", &[(false, false)]));
        for value in [
            row(vec![text("x"), DataValue::Int32(1), DataValue::Int32(1)]),
            row(vec![text("y"), DataValue::Int32(2), DataValue::Int32(2)]),
        ] {
            accumulator.update_value(&value)?;
        }
        assert_eq!(result(&mut accumulator)?, "y2 x1");

        // A tuple must hold at least one value besides the keys.
        assert!(accumulator
            .update_value(&row(vec![DataValue::Int32(1)]))
            .is_err());
        Ok(())
    }
}
// GRCOV_EXCL_STOP
