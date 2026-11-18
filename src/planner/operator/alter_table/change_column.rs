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
use crate::planner::MetaArena;
use crate::planner::{fmt_table, Explain, ExprRef};
use crate::types::LogicalType;
use kite_sql_serde_macros::ReferenceSerialization;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq, Clone, Hash, ReferenceSerialization)]
pub enum DefaultChange {
    NoChange,
    Set(ExprRef),
    Drop,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash, ReferenceSerialization)]
pub enum NotNullChange {
    NoChange,
    Set,
    Drop,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct ChangeColumnOperator {
    pub table_id: TableId,
    pub old_column_name: String,
    pub new_column_name: String,
    pub data_type: LogicalType,
    pub default_change: DefaultChange,
    pub not_null_change: NotNullChange,
    pub display_type: Option<Arc<str>>,
}

impl Explain for ChangeColumnOperator {
    fn fmt<A: MetaArena + ?Sized>(
        &self,
        arena: &A,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "Change {} -> ", self.old_column_name)?;
        fmt_table(self.table_id, arena, f)?;
        write!(f, ".{} ({}, ", self.new_column_name, self.data_type)?;
        match &self.default_change {
            DefaultChange::NoChange => f.write_str("NoChange")?,
            DefaultChange::Set(expr) => write!(f, "Set({})", expr.explain(arena))?,
            DefaultChange::Drop => f.write_str("Drop")?,
        }
        write!(f, ", {:?})", self.not_null_change)
    }
}

impl crate::serdes::ReferenceSerialization for ChangeColumnOperator {
    fn encode<W: std::io::Write, A: MetaArena + ?Sized>(
        &self,
        writer: &mut W,
        is_direct: bool,
        arena: &A,
    ) -> Result<(), crate::errors::DatabaseError> {
        #[allow(unused_imports)]
        use crate::serdes::ReferenceSerialization as _;
        self.table_id.encode(writer, is_direct, arena)?;
        self.old_column_name.encode(writer, is_direct, arena)?;
        self.new_column_name.encode(writer, is_direct, arena)?;
        self.data_type.encode(writer, is_direct, arena)?;
        self.default_change.encode(writer, is_direct, arena)?;
        self.not_null_change.encode(writer, is_direct, arena)?;
        self.display_type.encode(writer, is_direct, arena)?;
        Ok(())
    }

    fn decode<T: crate::storage::StorageTransaction, R: std::io::Read, A: MetaArena + ?Sized>(
        reader: &mut R,
        mut context: Option<&mut crate::serdes::ReferenceDecodeContext<'_, T>>,
        arena: &mut A,
    ) -> Result<Self, crate::errors::DatabaseError> {
        #[allow(unused_imports)]
        use crate::serdes::ReferenceSerialization as _;
        let table_id = TableId::decode(reader, context.as_deref_mut(), arena)?;
        let old_column_name = String::decode(reader, context.as_deref_mut(), arena)?;
        let new_column_name = String::decode(reader, context.as_deref_mut(), arena)?;
        let data_type = crate::types::LogicalType::decode(reader, context.as_deref_mut(), arena)?;
        let default_change = DefaultChange::decode(reader, context.as_deref_mut(), arena)?;
        let not_null_change = NotNullChange::decode(reader, context.as_deref_mut(), arena)?;

        let display_type = match Option::<Arc<str>>::decode(reader, context, arena) {
            Ok(dt) => dt,
            Err(crate::errors::DatabaseError::IO(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                None
            }
            Err(e) => return Err(e),
        };

        Ok(ChangeColumnOperator {
            table_id,
            old_column_name,
            new_column_name,
            data_type,
            default_change,
            not_null_change,
            display_type,
        })
    }
}
