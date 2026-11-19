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
use crate::execution::{DDLApply, ExecBuilder, ExecContext, ExecNode, ExecutorNode, WriteExecutor};
use crate::planner::operator::drop_table::DropTableOperator;
use crate::planner::{ExecMetaArena, MetaArena};
use crate::storage::StorageTransaction;

pub struct DropTable<'a> {
    op: &'a DropTableOperator,
}

impl<'a> WriteExecutor<'a> for DropTable<'a> {
    type Input = &'a DropTableOperator;

    fn into_executor<B: ExecBuilder>(
        op: Self::Input,
        _arena: &mut B,
        _plan_arena: &(dyn MetaArena + 'a),
    ) -> ExecNode<'a> {
        ExecNode::DropTable(Box::new(DropTable { op }))
    }
}

#[maybe_async]
impl<'a, T: StorageTransaction + 'a> ExecutorNode<'a, T> for DropTable<'a> {
    async fn reset<A: MetaArena + 'a>(
        &mut self,
        _arena: &mut ExecContext<'_, 'a, '_, T>,
        _plan_arena: &mut ExecMetaArena<A>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn next_tuple<A: MetaArena + 'a>(
        &mut self,
        arena: &mut ExecContext<'_, 'a, '_, T>,
        plan_arena: &mut ExecMetaArena<A>,
    ) -> Result<(), DatabaseError> {
        let DropTableOperator {
            table_names,
            if_exists,
        } = self.op;

        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for table_name in table_names.iter() {
            if !seen.insert(table_name.to_string()) {
                return Err(DatabaseError::DuplicateTableName(table_name.to_string()));
            }
        }

        {
            let (transaction, table_codec) = arena.transaction_codec_mut();

            if !if_exists {
                for table_name in table_names.iter() {
                    if transaction
                        .root_table(table_codec, table_name, plan_arena)
                        .await?
                        .filter(|meta| !meta.is_view)
                        .is_none()
                    {
                        return Err(DatabaseError::TableNotFound);
                    }
                }
            }
        }

        let mut dropped_tables = Vec::new();
        {
            let (transaction, table_codec) = arena.transaction_codec_mut();
            for table_name in table_names.iter() {
                if transaction
                    .drop_table(table_codec, plan_arena, table_name.clone(), true)
                    .await?
                {
                    dropped_tables.push(table_name.clone());
                }
            }
        }

        for name in dropped_tables {
            arena.push_ddl_apply(DDLApply::DropTable { name });
        }

        arena.finish();
        Ok(())
    }
}
