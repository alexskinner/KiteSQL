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
use crate::planner::operator::insert::InsertOperator;
use crate::planner::{ExecMetaArena, MetaArena};
use crate::planner::{ExprRef, LogicalPlan};
use crate::storage::{table_codec::TableCodec, StorageTransaction};
use crate::types::index::{Index, IndexType};
use crate::types::tuple::{Tuple, TupleImpl, TupleLike};
use crate::types::tuple_builder::TupleBuilder;
use crate::types::value::DataValue;
use std::mem;
use std::sync::Arc;

pub struct Insert<'a> {
    table: &'a DmlTable,
    input: ExecNode<'a>,
    tuple: TupleImpl,
    done: bool,
    is_overwrite: bool,
    on_duplicate_key_update: Option<&'a [(usize, ExprRef)]>,
}

impl<'a> WriteExecutor<'a> for Insert<'a> {
    type Input = (&'a InsertOperator, &'a LogicalPlan);

    fn into_executor<B: ExecBuilder>(
        (
            InsertOperator {
                table,
                is_overwrite,
                on_duplicate_key_update,
            },
            input_plan,
        ): Self::Input,
        arena: &mut B,
        plan_arena: &(dyn MetaArena + 'a),
    ) -> ExecNode<'a> {
        let input = build_read(arena, plan_arena, input_plan);
        ExecNode::Insert(Box::new(Insert {
            table,
            input,
            tuple: TupleImpl::from(Tuple::new(None, Vec::with_capacity(table.columns_len))),
            done: false,
            is_overwrite: *is_overwrite,
            on_duplicate_key_update: on_duplicate_key_update.as_deref(),
        }))
    }
}

#[maybe_async]
impl<'a, T: StorageTransaction + 'a> ExecutorNode<'a, T> for Insert<'a> {
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
        let Insert {
            table,
            input,
            tuple,
            is_overwrite,
            on_duplicate_key_update,
            ..
        } = self;
        if table.primary_key_indices.is_empty() {
            return Err(DatabaseError::not_null());
        }
        let mut affected_rows = 0;

        while input.next_tuple(arena, plan_arena).await? {
            tuple.clear();
            arena.result_mut().drain_values(|value| tuple.push(value));
            for (i, column) in table.columns.iter().enumerate() {
                let column = plan_arena.column(*column);
                let value = mem::replace(&mut tuple[i], DataValue::Null).cast(column.datatype())?;
                value.check_len(column.datatype())?;
                if value.is_null() && !column.nullable() {
                    return Err(DatabaseError::not_null_column(column.name().to_string()));
                }
                tuple[i] = value;
            }
            tuple.set_pk(Some(Tuple::primary_projection(
                &table.primary_key_indices,
                &*tuple,
            )));
            let enable_undo = arena.should_record_undo();
            let tuple_id = tuple.pk().ok_or(DatabaseError::PrimaryKeyNotFound)?;

            let conflict_tuple_id = if on_duplicate_key_update.is_some() {
                let mut conflict_id = None;

                // Check PRIMARY KEY for conflicts first
                let pk_exists = {
                    let mut state = arena.local_state(plan_arena);
                    let (transaction, table_codec) = state.transaction_codec_mut();
                    table_codec
                        .with_tuple(table.table_id, tuple_id, None, async |key, _| {
                            transaction.exists(key).await
                        })
                        .await?
                };

                if pk_exists {
                    conflict_id = Some(tuple_id.clone());
                } else {
                    // Check UNIQUE indexes for conflicts
                    for (index_meta, exprs) in table.index_exprs.iter() {
                        let index_meta = plan_arena.index(*index_meta);
                        if !matches!(index_meta.ty, IndexType::Unique) {
                            continue;
                        }

                        arena.rewrite(exprs, plan_arena, Some(&*tuple))?;
                        let mut state = arena.local_state(plan_arena);
                        let (values, transaction, table_codec) =
                            state.tuple_transaction_codec_mut();
                        // As in MySQL, a NULL in a UNIQUE key never equals another row's key.
                        if (0..values.len()).any(|i| values.value_at(i).is_null()) {
                            continue;
                        }
                        let index = Index::new(index_meta.id, values, index_meta.ty);

                        let existing_id = table_codec
                            .with_index(table.table_id, &index, Some(tuple_id), async |key, _| {
                                if let Some(bytes) = transaction.get_borrowed(key).await? {
                                    let existing_tuple_id =
                                        TableCodec::decode_index(bytes.as_ref())?;
                                    if &existing_tuple_id != tuple_id {
                                        return Ok(Some(existing_tuple_id));
                                    }
                                }
                                Ok(None)
                            })
                            .await?;

                        if existing_id.is_some() {
                            conflict_id = existing_id;
                            break;
                        }
                    }
                }

                conflict_id
            } else {
                None
            };

            if let Some(conflict_tuple_id) = conflict_tuple_id {
                let update_exprs = on_duplicate_key_update.unwrap_or_default();

                let mut existing_tuple = Tuple::new(Some(conflict_tuple_id.clone()), Vec::new());
                let mut state = arena.local_state(plan_arena);
                let (transaction, table_codec) = state.transaction_codec_mut();
                let found = table_codec
                    .with_tuple_unchecked(
                        table.table_id,
                        &conflict_tuple_id,
                        None,
                        async |key, _, pool| {
                            let Some(bytes) = transaction.get_borrowed(key).await? else {
                                return Ok(false);
                            };
                            // SAFETY: `bytes` is a row this crate encoded, read from
                            // `transaction`. Without a mapped scope the values are copied, so the
                            // row stays valid across the writes below.
                            unsafe {
                                TableCodec::decode_tuple_into(
                                    &mut existing_tuple,
                                    table.serializers.iter(),
                                    Some(conflict_tuple_id.clone()),
                                    bytes.as_ref(),
                                    table.columns.len(),
                                    pool,
                                    None,
                                )?
                            };
                            Ok(true)
                        },
                    )
                    .await?;
                if !found {
                    return Err(DatabaseError::TupleIdNotFound(conflict_tuple_id));
                }
                let old_tuple = TupleImpl::from(existing_tuple);

                // Each SET expression reads the existing row followed by the row the
                // statement tried to insert (`VALUES(col)` / the row alias).
                let combined_tuple = TupleImpl::concat(
                    old_tuple.clone(),
                    TupleImpl::from(Tuple::new(
                        tuple.pk().cloned(),
                        tuple.iter().cloned().collect(),
                    )),
                    Some(conflict_tuple_id.clone()),
                );
                let mut new_tuple = old_tuple.clone();
                for (column_index, expr) in update_exprs {
                    let column = plan_arena.column(table.columns[*column_index]);
                    let value = plan_arena
                        .expression(*expr)
                        .eval(plan_arena, Some(&combined_tuple))?
                        .into_owned()
                        .cast(column.datatype())?;
                    value.check_len(column.datatype())?;
                    if value.is_null() && !column.nullable() {
                        return Err(DatabaseError::not_null_column(column.name().to_string()));
                    }
                    new_tuple[*column_index] = value;
                }
                // MySQL reports 2 for a changed row and 0 for an unchanged one.
                if new_tuple.iter().eq(old_tuple.iter()) {
                    continue;
                }

                for (index_meta, exprs) in table.index_exprs.iter() {
                    let index_meta = plan_arena.index(*index_meta);
                    arena.rewrite(exprs, plan_arena, Some(&old_tuple))?;
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
                        .del_index(table_codec, table.table_id, &old_index, &conflict_tuple_id)
                        .await?;
                    if let Some(index_values) = old_index_values {
                        arena.undo_log().record_add_index(
                            table.table_id,
                            index_meta.id,
                            index_meta.ty,
                            index_values,
                            conflict_tuple_id.clone(),
                        );
                    }
                }

                let new_pk = Tuple::primary_projection(&table.primary_key_indices, &new_tuple);
                let primary_key_changed = new_pk != conflict_tuple_id;
                let serializers = enable_undo.then(|| Arc::new(table.serializers.clone()));
                if primary_key_changed {
                    // The row moves: delete it under the old key, insert it under the new one.
                    let mut state = arena.local_state(plan_arena);
                    let (transaction, table_codec) = state.transaction_codec_mut();
                    transaction
                        .remove_tuple(table_codec, table.table_id, &conflict_tuple_id)
                        .await?;
                    if let Some(serializers) = &serializers {
                        arena.undo_log().record_append_tuple(
                            table.table_id,
                            old_tuple.clone(),
                            Arc::clone(serializers),
                        );
                    }
                }

                for (index_meta, exprs) in table.index_exprs.iter() {
                    let index_meta = plan_arena.index(*index_meta);
                    arena.rewrite(exprs, plan_arena, Some(&new_tuple))?;
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

                new_tuple.set_pk(Some(new_pk.clone()));
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
                                &new_tuple,
                                &table.serializers,
                                is_overwrite,
                            )
                            .await
                    })
                    .await?;
                if let Some(serializers) = serializers {
                    if primary_key_changed {
                        arena.undo_log().record_remove_tuple(table.table_id, new_pk);
                    } else {
                        arena.undo_log().record_append_tuple_overwrite(
                            table.table_id,
                            old_tuple,
                            serializers,
                        );
                    }
                }

                affected_rows += 2;
            } else {
                for (index_meta, exprs) in table.index_exprs.iter() {
                    let index_meta = plan_arena.index(*index_meta);
                    arena.rewrite(exprs, plan_arena, Some(&*tuple))?;
                    let index_values = if enable_undo {
                        let mut state = arena.local_state(plan_arena);
                        let (values, _, _) = state.tuple_transaction_codec_mut();
                        Some(values.clone())
                    } else {
                        None
                    };
                    let mut state = arena.local_state(plan_arena);
                    let (values, transaction, table_codec) = state.tuple_transaction_codec_mut();
                    let index = Index::new(index_meta.id, values, index_meta.ty);
                    transaction
                        .add_index(table_codec, table.table_id, index, tuple_id)
                        .await?;
                    if let Some(index_values) = index_values {
                        arena.undo_log().record_delete_index(
                            table.table_id,
                            index_meta.id,
                            index_meta.ty,
                            index_values,
                            tuple_id.clone(),
                        );
                    }
                }
                let mut state = arena.local_state(plan_arena);
                let (transaction, table_codec) = state.transaction_codec_mut();
                transaction
                    .append_tuple(
                        table_codec,
                        table.table_id,
                        &*tuple,
                        &table.serializers,
                        *is_overwrite,
                    )
                    .await?;
                if enable_undo {
                    arena
                        .undo_log()
                        .record_remove_tuple(table.table_id, tuple_id.clone());
                }
                affected_rows += 1;
            }
        }
        arena.produce(TupleBuilder::build_result(affected_rows.to_string()).into());
        arena.resume();
        Ok(())
    }
}
