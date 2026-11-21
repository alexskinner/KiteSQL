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

use crate::catalog::table::DmlTable;
use crate::planner::{fmt_table, Explain, ExprRef, MetaArena};
use kite_sql_serde_macros::ReferenceSerialization;

#[derive(Debug, PartialEq, Eq, Clone, Hash, ReferenceSerialization)]
pub struct InsertOperator {
    pub(crate) table: DmlTable,
    pub is_overwrite: bool,
    pub on_duplicate_key_update: Option<Vec<(usize, ExprRef)>>,
    /// The table's identity column, whose omitted or NULL values the insert generates.
    pub identity: Option<IdentityTarget>,
    pub upsert_update_count: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, ReferenceSerialization)]
pub struct IdentityTarget {
    /// Position of the identity column in the table's columns.
    pub index: usize,
    /// Whether an explicit 0 also generates a value (see `SqlDialect::identity_zero_generates`).
    pub zero_generates: bool,
}

impl Explain for InsertOperator {
    fn fmt<A: MetaArena + ?Sized>(
        &self,
        arena: &A,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.write_str("Insert ")?;
        fmt_table(self.table.table_id, arena, f)?;
        write!(f, ", Is Overwrite: {}", self.is_overwrite)?;
        if self.on_duplicate_key_update.is_some() {
            f.write_str(", On Duplicate Key Update: enabled")?;
        }
        Ok(())
    }
}
