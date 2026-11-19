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

use kite_sql_serde_macros::ReferenceSerialization;

#[derive(Debug, Clone, PartialEq, Eq, Hash, ReferenceSerialization)]
pub enum AggKind {
    Avg,
    Max,
    Min,
    Sum,
    Count,
    /// SQLite `total()`: a float sum that is 0.0, not NULL, over no rows.
    Total,
    /// SQLite `group_concat()`: non-NULL values joined with `,`.
    GroupConcat,
    /// User-defined aggregate function, identified by name and input type
    Udf(AggFnId),
    /// `group_concat()` with a separator other than `,`, or with an ORDER BY. Its arguments are
    /// the value followed by one sort key per [`GroupConcatOptions::order_by`] entry.
    /// (A new variant, so plans stored with plain [`AggKind::GroupConcat`] still decode.)
    GroupConcatWith(GroupConcatOptions),
}

/// How `group_concat()` joins its values: the separator, and the order of the values.
#[derive(Debug, Clone, PartialEq, Eq, Hash, ReferenceSerialization)]
pub struct GroupConcatOptions {
    pub separator: String,
    pub order_by: Vec<GroupConcatOrder>,
}

/// The direction of one `group_concat()` sort key, as in a query's ORDER BY.
#[derive(Debug, Clone, PartialEq, Eq, Hash, ReferenceSerialization)]
pub struct GroupConcatOrder {
    pub asc: bool,
    pub nulls_first: bool,
}

impl GroupConcatOptions {
    pub const DEFAULT_SEPARATOR: &'static str = ",";

    /// The aggregate kind for these options: plain [`AggKind::GroupConcat`] for the default.
    pub fn into_kind(self) -> AggKind {
        if self.separator == Self::DEFAULT_SEPARATOR && self.order_by.is_empty() {
            AggKind::GroupConcat
        } else {
            AggKind::GroupConcatWith(self)
        }
    }
}

/// Identifier for a user-defined aggregate function
#[derive(Debug, Clone, PartialEq, Eq, Hash, ReferenceSerialization)]
pub struct AggFnId {
    pub name: String,
    pub arg_types: Vec<crate::types::LogicalType>,
}

impl AggKind {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "avg" => Some(Self::Avg),
            "max" => Some(Self::Max),
            "min" => Some(Self::Min),
            "sum" => Some(Self::Sum),
            "count" => Some(Self::Count),
            "total" => Some(Self::Total),
            "group_concat" => Some(Self::GroupConcat),
            _ => None,
        }
    }

    pub(crate) fn name(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Avg => "avg".into(),
            Self::Max => "max".into(),
            Self::Min => "min".into(),
            Self::Sum => "sum".into(),
            Self::Count => "count".into(),
            Self::Total => "total".into(),
            Self::GroupConcat | Self::GroupConcatWith(_) => "group_concat".into(),
            Self::Udf(id) => id.name.clone().into(),
        }
    }

    pub fn allow_distinct(&self) -> bool {
        match self {
            AggKind::Avg => true,
            AggKind::Max => false,
            AggKind::Min => false,
            AggKind::Sum => true,
            AggKind::Count => true,
            AggKind::Total => true,
            AggKind::GroupConcat | AggKind::GroupConcatWith(_) => true,
            AggKind::Udf(_) => false, // Default: no DISTINCT for UDAFs
        }
    }
}

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{AggKind, GroupConcatOptions, GroupConcatOrder};

    #[test]
    fn test_group_concat_options_into_kind() {
        let plain = GroupConcatOptions {
            separator: ",".to_string(),
            order_by: Vec::new(),
        };
        // The default stays the original kind, so stored plans keep their encoding.
        assert_eq!(plain.clone().into_kind(), AggKind::GroupConcat);

        let separator = GroupConcatOptions {
            separator: "|".to_string(),
            ..plain.clone()
        };
        assert_eq!(
            separator.clone().into_kind(),
            AggKind::GroupConcatWith(separator)
        );
        let ordered = GroupConcatOptions {
            order_by: vec![GroupConcatOrder {
                asc: false,
                nulls_first: false,
            }],
            ..plain
        };
        let kind = ordered.clone().into_kind();
        assert_eq!(kind, AggKind::GroupConcatWith(ordered));
        assert_eq!(kind.name(), "group_concat");
        assert!(kind.allow_distinct());
    }
}
// GRCOV_EXCL_STOP
