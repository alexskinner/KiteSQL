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
use crate::errors::DatabaseError;
use crate::execution::{
    build_read, ExecBuilder, ExecContext, ExecNode, ExecutorNode, WriteExecutor,
};
use crate::planner::operator::update::UpdateOperator;
use crate::planner::ExprRef;
use crate::planner::LogicalPlan;
use crate::planner::{ExecMetaArena, MetaArena};
use crate::storage::StorageTransaction;
use crate::types::index::Index;
use crate::types::tuple::{Tuple, TupleId};
use crate::types::tuple_builder::TupleBuilder;
use std::collections::HashSet;
use std::mem;
use std::sync::Arc;

pub struct Update<'a> {
    table: &'a DmlTable,
    value_exprs: &'a [(usize, ExprRef)],
    target_table_offset: usize,
    input: ExecNode<'a>,
    done: bool,
}

impl<'a> WriteExecutor<'a> for Update<'a> {
    type Input = (&'a UpdateOperator, &'a LogicalPlan);

    fn into_executor<B: ExecBuilder>(
        (
            UpdateOperator {
                table,
                value_exprs,
                target_table_offset,
            },
            input_plan,
        ): Self::Input,
        arena: &mut B,
        plan_arena: &(dyn MetaArena + 'a),
    ) -> ExecNode<'a> {
        let input = build_read(arena, plan_arena, input_plan);
        ExecNode::Update(Box::new(Update {
            table,
            value_exprs,
            target_table_offset: *target_table_offset,
            input,
            done: false,
        }))
    }
}

#[maybe_async]
impl<'a, T: StorageTransaction + 'a> ExecutorNode<'a, T> for Update<'a> {
    async fn reset<A: MetaArena + 'a>(
        &mut self,
        arena: &mut ExecContext<'_, 'a, '_, T>,
        plan_arena: &mut ExecMetaArena<A>,
    ) -> Result<(), DatabaseError> {
        self.done = false;
        self.input.reset(arena, plan_arena).await
    }

    async fn next_tuple<A: MetaArena + 'a>(
        &mut self,
        arena: &mut ExecContext<'_, 'a, '_, T>,
        plan_arena: &mut ExecMetaArena<A>,
    ) -> Result<(), DatabaseError> {
        if mem::replace(&mut self.done, true) {
            arena.finish();
            return Ok(());
        }
        let Update {
            table,
            value_exprs,
            target_table_offset,
            input,
            ..
        } = self;
        let mut updated_count = 0;
        let mut values = Vec::with_capacity(value_exprs.len());

        let enable_undo = arena.should_record_undo();
        let serializers_arc = if enable_undo {
            Some(Arc::new(table.serializers.clone()))
        } else {
            None
        };

        let mut updated_pks: Option<HashSet<TupleId>> = None;

        while input.next_tuple(arena, plan_arena).await? {
            let joined_tuple = arena.take_tuple();

            let is_simple_update =
                *target_table_offset == 0 && joined_tuple.len() == table.columns.len();

            if !is_simple_update && updated_pks.is_none() {
                updated_pks = Some(HashSet::new());
            }

            let (mut tuple, eval_tuple_owned, old_pk) = if is_simple_update {
                let pk = joined_tuple.pk().cloned();
                (joined_tuple, None, pk)
            } else {
                let target_values: Vec<_> = (0..table.columns.len())
                    .map(|i| joined_tuple[*target_table_offset + i].clone())
                    .collect();

                if target_values.iter().all(|v| v.is_null()) {
                    continue;
                }

                let target_pk =
                    Tuple::primary_projection(&table.primary_key_indices, &target_values[..]);
                let tuple = Tuple {
                    pk: Some(target_pk.clone()),
                    values: target_values,
                }
                .into();
                (tuple, Some(joined_tuple), Some(target_pk))
            };

            let Some(old_pk) = old_pk else {
                continue;
            };

            if let Some(ref mut pks) = updated_pks {
                if !pks.insert(old_pk.clone()) {
                    continue;
                }
            }

            // Capture old tuple for undo (before modification)
            let old_tuple = if enable_undo {
                Some(tuple.clone())
            } else {
                None
            };

            // Delete old indexes and record how to restore them
            for (index_meta, exprs) in table.index_exprs.iter() {
                let index_meta = plan_arena.index(*index_meta);
                arena.rewrite(exprs, plan_arena, Some(&tuple))?;
                let old_index_values = if enable_undo {
                    let mut state = arena.local_state(plan_arena);
                    let (values, _, _) = state.tuple_transaction_codec_mut();
                    Some(values.clone())
                } else {
                    None
                };
                let mut state = arena.local_state(plan_arena);
                let (values, transaction, table_codec) = state.tuple_transaction_codec_mut();
                let old_index = Index::new(index_meta.id, values, index_meta.ty);
                transaction
                    .del_index(table_codec, table.table_id, &old_index, &old_pk)
                    .await?;
                if let Some(index_values) = old_index_values {
                    arena.undo_log().record_add_index(
                        table.table_id,
                        index_meta.id,
                        index_meta.ty,
                        index_values,
                        old_pk.clone(),
                    );
                }
            }

            // Every expression reads the old row (or joined row for joined updates), so evaluate them all before writing.
            for (_, expr) in value_exprs.iter() {
                let eval_tuple = eval_tuple_owned.as_ref().unwrap_or(&tuple);
                let value = plan_arena
                    .expression(*expr)
                    .eval(plan_arena, Some(eval_tuple))?;
                values.push(value.into_owned());
            }
            for ((position, _), value) in value_exprs.iter().zip(values.drain(..)) {
                tuple[*position] = value;
            }

            let new_pk = Tuple::primary_projection(&table.primary_key_indices, &tuple);
            let primary_key_changed = new_pk != old_pk;
            if primary_key_changed {
                let mut state = arena.local_state(plan_arena);
                let (transaction, table_codec) = state.transaction_codec_mut();
                transaction
                    .remove_tuple(table_codec, table.table_id, &old_pk)
                    .await?;
                if let (Some(ref old_tuple), Some(ref serializers)) = (&old_tuple, &serializers_arc)
                {
                    arena.undo_log().record_append_tuple(
                        table.table_id,
                        old_tuple.clone(),
                        Arc::clone(serializers),
                    );
                }
            }

            // Add new indexes and record how to remove them
            for (index_meta, exprs) in table.index_exprs.iter() {
                let index_meta = plan_arena.index(*index_meta);
                arena.rewrite(exprs, plan_arena, Some(&tuple))?;
                let new_index_values = if enable_undo {
                    let mut state = arena.local_state(plan_arena);
                    let (values, _, _) = state.tuple_transaction_codec_mut();
                    Some(values.clone())
                } else {
                    None
                };
                let mut state = arena.local_state(plan_arena);
                let (values, transaction, table_codec) = state.tuple_transaction_codec_mut();
                let new_index = Index::new(index_meta.id, values, index_meta.ty);
                transaction
                    .add_index(table_codec, table.table_id, new_index, &new_pk)
                    .await?;
                if let Some(index_values) = new_index_values {
                    arena.undo_log().record_delete_index(
                        table.table_id,
                        index_meta.id,
                        index_meta.ty,
                        index_values,
                        new_pk.clone(),
                    );
                }
            }

            tuple.set_pk(Some(new_pk.clone()));

            let mut state = arena.local_state(plan_arena);
            let (transaction, table_codec) = state.transaction_codec_mut();
            let is_overwrite = !primary_key_changed;
            let stamp = if is_overwrite { 0 } else { table_codec.stamp() };
            table_codec
                .with_stamp(stamp, async |table_codec| {
                    transaction
                        .append_tuple(
                            table_codec,
                            table.table_id,
                            &tuple,
                            &table.serializers,
                            is_overwrite,
                        )
                        .await
                })
                .await?;

            // Record how to undo the append
            if let (Some(old_tuple), Some(ref serializers)) = (old_tuple, &serializers_arc) {
                if primary_key_changed {
                    arena
                        .undo_log()
                        .record_remove_tuple(table.table_id, new_pk.clone());
                } else {
                    arena.undo_log().record_append_tuple_overwrite(
                        table.table_id,
                        old_tuple,
                        Arc::clone(serializers),
                    );
                }
            }

            updated_count += 1;
        }
        arena.produce(TupleBuilder::build_result(updated_count.to_string()).into());
        arena.resume();
        Ok(())
    }
}
