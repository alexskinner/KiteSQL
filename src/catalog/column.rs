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

use crate::catalog::TableId;
use crate::errors::DatabaseError;
use crate::planner::ExprRef;
use crate::planner::MetaArena;
use crate::types::value::DataValue;
use crate::types::CharLengthUnits;
use crate::types::{ColumnId, LogicalType};
use kite_sql_serde_macros::ReferenceSerialization;
use std::borrow::Cow;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct ColumnRef {
    pos: usize,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, ReferenceSerialization)]
pub struct ColumnCatalog {
    summary: ColumnSummary,
    nullable: bool,
    desc: ColumnDesc,
    in_join: bool,
}

pub type SourceId = usize;

#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum ColumnRelation {
    None,
    /// A column of a stored table.
    Table {
        table_id: TableId,
        column_id: ColumnId,
    },
    /// A column of a FROM item other than a stored table: an alias, a derived table, a CTE,
    /// VALUES or a view. Every binding of an item gets its own `source_id`, and the arena keeps
    /// its name.
    Source {
        source_id: SourceId,
        column_id: ColumnId,
        is_temp: bool,
    },
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, ReferenceSerialization)]
pub struct ColumnSummary {
    pub name: String,
    pub relation: ColumnRelation,
}

impl ColumnRelation {
    /// Whether both belong to the same table or FROM item.
    pub(crate) fn same_relation(&self, other: &Self) -> bool {
        match (self, other) {
            (
                ColumnRelation::Table { table_id, .. },
                ColumnRelation::Table {
                    table_id: other, ..
                },
            ) => table_id == other,
            (
                ColumnRelation::Source { source_id, .. },
                ColumnRelation::Source {
                    source_id: other, ..
                },
            ) => source_id == other,
            _ => false,
        }
    }
}

impl ColumnRef {
    pub(crate) fn new(pos: usize) -> Self {
        Self { pos }
    }

    pub(crate) fn pos(self) -> usize {
        self.pos
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.pos)
    }
}

impl ColumnCatalog {
    pub fn new(column_name: String, nullable: bool, column_desc: ColumnDesc) -> ColumnCatalog {
        ColumnCatalog {
            summary: ColumnSummary {
                name: column_name,
                relation: ColumnRelation::None,
            },
            nullable,
            desc: column_desc,
            in_join: false,
        }
    }

    pub(crate) fn new_dummy(column_name: String) -> ColumnCatalog {
        ColumnCatalog {
            summary: ColumnSummary {
                name: column_name,
                relation: ColumnRelation::None,
            },
            nullable: true,
            // SAFETY: default expr must not be [`ScalarExpression::ColumnRef`]
            desc: ColumnDesc::new(
                LogicalType::Varchar(None, CharLengthUnits::Characters),
                None,
                false,
                None,
            )
            .unwrap(),
            in_join: false,
        }
    }

    pub(crate) fn summary(&self) -> &ColumnSummary {
        &self.summary
    }

    pub fn summary_mut(&mut self) -> &mut ColumnSummary {
        &mut self.summary
    }

    pub fn id(&self) -> Option<ColumnId> {
        match &self.summary.relation {
            ColumnRelation::None => None,
            ColumnRelation::Table { column_id, .. } | ColumnRelation::Source { column_id, .. } => {
                Some(*column_id)
            }
        }
    }

    pub(crate) fn table_id(&self) -> Option<TableId> {
        match &self.summary.relation {
            ColumnRelation::Table { table_id, .. } => Some(*table_id),
            _ => None,
        }
    }

    pub fn name(&self) -> &str {
        &self.summary.name
    }

    pub fn full_name(&self, arena: &(impl MetaArena + ?Sized)) -> String {
        let name = self.name();
        match &self.summary.relation {
            ColumnRelation::None => name.to_string(),
            ColumnRelation::Table { table_id, .. } => match arena.table_name(*table_id) {
                Some(table_name) => format!("{table_name}.{name}"),
                None => format!("#{table_id}.{name}"),
            },
            ColumnRelation::Source { source_id, .. } => format!("#{source_id}.{name}"),
        }
    }

    pub(crate) fn is_persistent_table_column(&self) -> bool {
        matches!(self.summary.relation, ColumnRelation::Table { .. })
    }

    pub fn set_name(&mut self, name: String) {
        self.summary.name = name;
    }

    pub fn set_table(&mut self, table_id: TableId, column_id: ColumnId) {
        self.summary.relation = ColumnRelation::Table {
            table_id,
            column_id,
        };
    }

    pub fn set_source(&mut self, source_id: SourceId, column_id: ColumnId, is_temp: bool) {
        self.summary.relation = ColumnRelation::Source {
            source_id,
            column_id,
            is_temp,
        };
    }

    pub fn in_join(&self) -> bool {
        self.in_join
    }

    pub fn nullable(&self) -> bool {
        self.nullable
    }

    pub(crate) fn set_nullable(&mut self, nullable: bool) {
        self.nullable = nullable;
    }

    pub(crate) fn set_in_join(&mut self, in_join: bool) {
        self.in_join = in_join;
    }

    pub fn datatype(&self) -> &LogicalType {
        &self.desc.column_datatype
    }

    pub(crate) fn default_value(
        &self,
        arena: &(dyn MetaArena + '_),
    ) -> Result<Option<DataValue>, DatabaseError> {
        self.desc
            .default
            .as_ref()
            .map(|expr| {
                arena
                    .expression(*expr)
                    .eval::<[DataValue]>(arena, None)
                    .map(Cow::into_owned)
            })
            .transpose()
    }

    pub(crate) fn desc(&self) -> &ColumnDesc {
        &self.desc
    }

    pub(crate) fn desc_mut(&mut self) -> &mut ColumnDesc {
        &mut self.desc
    }
}

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
mod tests {
    use super::{ColumnCatalog, ColumnDesc};
    use crate::errors::DatabaseError;
    use crate::planner::PlanArena;
    use crate::types::LogicalType;

    #[test]
    fn test_same_column_ignores_nullable_and_desc() -> Result<(), DatabaseError> {
        let mut left = ColumnCatalog::new(
            "c1".to_string(),
            false,
            ColumnDesc::new(LogicalType::Integer, None, false, None)?,
        );
        let mut right = ColumnCatalog::new(
            "c1".to_string(),
            true,
            ColumnDesc::new(LogicalType::Bigint, None, false, None)?,
        );
        let table_arena = crate::planner::TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);
        let left_ref = arena.alloc_column(left.clone());
        let right_ref = arena.alloc_column(right.clone());

        assert_ne!(left_ref, right_ref);
        assert!(arena.same_column(left_ref, right_ref));

        left.set_name("c2".to_string());
        right.set_name("c3".to_string());
        let left_ref = arena.alloc_column(left);
        let right_ref = arena.alloc_column(right);
        assert!(!arena.same_column(left_ref, right_ref));
        Ok(())
    }
}
// GRCOV_EXCL_STOP

/// The descriptor of a column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnDesc {
    pub(crate) column_datatype: LogicalType,
    primary: Option<usize>,
    is_unique: bool,
    pub(crate) default: Option<ExprRef>,
    pub(crate) display_type: Option<std::sync::Arc<str>>,
}

impl ColumnDesc {
    pub fn new(
        column_datatype: LogicalType,
        primary: Option<usize>,
        is_unique: bool,
        default: Option<ExprRef>,
    ) -> Result<ColumnDesc, DatabaseError> {
        Ok(ColumnDesc {
            column_datatype,
            display_type: None,
            primary,
            is_unique,
            default,
        })
    }

    pub fn with_display_type(mut self, display_type: Option<std::sync::Arc<str>>) -> Self {
        self.display_type = display_type;
        self
    }

    pub fn display_type(&self) -> Option<&str> {
        self.display_type.as_deref()
    }

    pub(crate) fn primary(&self) -> Option<usize> {
        self.primary
    }

    pub(crate) fn is_primary(&self) -> bool {
        self.primary.is_some()
    }

    pub(crate) fn set_primary(&mut self, is_primary: Option<usize>) {
        self.primary = is_primary
    }

    pub(crate) fn is_unique(&self) -> bool {
        self.is_unique
    }

    pub(crate) fn set_unique(&mut self) {
        self.is_unique = true
    }
}

impl crate::serdes::ReferenceSerialization for ColumnDesc {
    fn encode<W: std::io::Write, A: crate::planner::MetaArena + ?Sized>(
        &self,
        writer: &mut W,
        is_direct: bool,
        arena: &A,
    ) -> Result<(), DatabaseError> {
        #[allow(unused_imports)]
        use crate::serdes::ReferenceSerialization as _;
        self.column_datatype.encode(writer, is_direct, arena)?;
        self.primary.encode(writer, is_direct, arena)?;
        self.is_unique.encode(writer, is_direct, arena)?;
        self.default.encode(writer, is_direct, arena)?;
        self.display_type.encode(writer, is_direct, arena)?;
        Ok(())
    }

    fn decode<
        T: crate::storage::StorageTransaction,
        R: std::io::Read,
        A: crate::planner::MetaArena + ?Sized,
    >(
        reader: &mut R,
        mut context: Option<&mut crate::serdes::ReferenceDecodeContext<'_, T>>,
        arena: &mut A,
    ) -> Result<Self, DatabaseError> {
        #[allow(unused_imports)]
        use crate::serdes::ReferenceSerialization as _;
        let column_datatype = LogicalType::decode(reader, context.as_deref_mut(), arena)?;
        let primary = Option::<usize>::decode(reader, context.as_deref_mut(), arena)?;
        let is_unique = bool::decode(reader, context.as_deref_mut(), arena)?;
        let default = Option::<ExprRef>::decode(reader, context.as_deref_mut(), arena)?;

        let display_type = match Option::<Arc<str>>::decode(reader, context, arena) {
            Ok(dt) => dt,
            Err(DatabaseError::IO(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(e) => return Err(e),
        };

        Ok(ColumnDesc {
            column_datatype,
            primary,
            is_unique,
            default,
            display_type,
        })
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
mod backward_compatibility_tests {
    use super::{ColumnDesc, LogicalType};
    use crate::errors::DatabaseError;
    use crate::planner::{PlanArena, TableArenaCell};
    use crate::serdes::ReferenceSerialization as _;

    #[test]
    fn test_column_desc_without_display_type() -> Result<(), DatabaseError> {
        use crate::storage::TestTransaction;

        let table_arena = TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);

        let old_desc = ColumnDesc::new(LogicalType::Integer, Some(0), false, None)?;

        let mut encode_buf = Vec::new();
        old_desc.encode(&mut encode_buf, false, &arena)?;

        let mut decode_cursor = &encode_buf[..];
        let decoded: ColumnDesc =
            ColumnDesc::decode::<TestTransaction, _, _>(&mut decode_cursor, None, &mut arena)?;

        assert_eq!(decoded.column_datatype, LogicalType::Integer);
        assert_eq!(decoded.primary, Some(0));
        assert!(!decoded.is_unique);
        assert_eq!(decoded.default, None);
        assert!(decoded.display_type.is_none());

        Ok(())
    }

    #[test]
    fn test_column_desc_with_display_type() -> Result<(), DatabaseError> {
        use crate::storage::TestTransaction;

        let table_arena = TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);

        let new_desc = ColumnDesc::new(LogicalType::Integer, None, false, None)?
            .with_display_type(Some("custom_int".into()));

        let mut encode_buf = Vec::new();
        new_desc.encode(&mut encode_buf, false, &arena)?;

        let mut decode_cursor = &encode_buf[..];
        let decoded: ColumnDesc =
            ColumnDesc::decode::<TestTransaction, _, _>(&mut decode_cursor, None, &mut arena)?;

        assert_eq!(decoded.column_datatype, LogicalType::Integer);
        assert_eq!(decoded.display_type.as_deref(), Some("custom_int"));

        Ok(())
    }

    #[test]
    fn test_old_format_bytes_decode() -> Result<(), DatabaseError> {
        use crate::storage::TestTransaction;

        let table_arena = TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);

        let old_desc_no_display = ColumnDesc {
            column_datatype: LogicalType::Bigint,
            primary: None,
            is_unique: true,
            default: None,
            display_type: None,
        };

        let mut old_format_bytes = Vec::new();
        old_desc_no_display
            .column_datatype
            .encode(&mut old_format_bytes, false, &arena)?;
        old_desc_no_display
            .primary
            .encode(&mut old_format_bytes, false, &arena)?;
        old_desc_no_display
            .is_unique
            .encode(&mut old_format_bytes, false, &arena)?;
        old_desc_no_display
            .default
            .encode(&mut old_format_bytes, false, &arena)?;

        let mut decode_cursor = &old_format_bytes[..];
        let decoded: ColumnDesc =
            ColumnDesc::decode::<TestTransaction, _, _>(&mut decode_cursor, None, &mut arena)?;

        assert_eq!(decoded.column_datatype, LogicalType::Bigint);
        assert_eq!(decoded.primary, None);
        assert!(decoded.is_unique);
        assert_eq!(decoded.default, None);
        assert!(decoded.display_type.is_none());

        Ok(())
    }
}
