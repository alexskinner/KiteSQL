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
use crate::storage::StorageTransaction;
use crate::types::index::Index;
use crate::types::tuple::{Tuple, TupleImpl};
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

            let conflict_detected = if on_duplicate_key_update.is_some() {
                let mut state = arena.local_state(plan_arena);
                let (transaction, table_codec) = state.transaction_codec_mut();
                table_codec
                    .with_tuple(
                        table.table_id,
                        tuple_id,
                        None,
                        async |key, _| transaction.exists(key),
                    )
                    .await?
            } else {
                false
            };

            if conflict_detected {
                let update_exprs = on_duplicate_key_update.unwrap();

                let mut existing_tuple = Tuple::new(Some(tuple_id.clone()), Vec::new());
                let mut state = arena.local_state(plan_arena);
                let (transaction, table_codec) = state.transaction_codec_mut();

                let found = {
                    table_codec
                        .with_tuple_unchecked(
                            table.table_id,
                            tuple_id,
                            None,
                            async |key, _, pool| {
                                let Some(bytes) = transaction.get_borrowed(key).await? else {
                                    return Ok(false);
                                };
                                unsafe {
                                    crate::storage::table_codec::TableCodec::decode_tuple_into(
                                        &mut existing_tuple,
                                        table.serializers.iter(),
                                        Some(tuple_id.clone()),
                                        bytes.as_ref(),
                                        table.columns.len(),
                                        pool,
                                        transaction.mapped_scope(),
                                    )?
                                };
                                Ok(true)
                            },
                        )
                        .await?
                };

                if !found {
                    return Err(DatabaseError::UnsupportedStmt(
                        "conflicting tuple not found".to_string(),
                    ));
                }

                let old_tuple_for_undo = if enable_undo {
                    Some(TupleImpl::from(existing_tuple.clone()))
                } else {
                    None
                };

                let serializers_arc = if enable_undo {
                    Some(Arc::new(table.serializers.clone()))
                } else {
                    None
                };

                for (index_meta, exprs) in table.index_exprs.iter() {
                    let index_meta = plan_arena.index(*index_meta);
                    arena.rewrite(exprs, plan_arena, Some(&existing_tuple))?;
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
                        .del_index(table_codec, table.table_id, &old_index, tuple_id)
                        .await?;
                    if let Some(index_values) = old_index_values {
                        arena.undo_log().record_add_index(
                            table.table_id,
                            index_meta.id,
                            index_meta.ty,
                            index_values,
                            tuple_id.clone(),
                        );
                    }
                }

                let mut existing_tuple_impl = TupleImpl::from(existing_tuple);
                for (column_index, expr) in update_exprs {
                    let value = plan_arena
                        .expression(*expr)
                        .eval(plan_arena, Some(&existing_tuple_impl))?;
                    existing_tuple_impl[*column_index] = value.into_owned();
                }

                for (index_meta, exprs) in table.index_exprs.iter() {
                    let index_meta = plan_arena.index(*index_meta);
                    arena.rewrite(exprs, plan_arena, Some(&existing_tuple_impl))?;
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
                        .add_index(table_codec, table.table_id, new_index, tuple_id)
                        .await?;
                    if let Some(index_values) = new_index_values {
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
                        &existing_tuple_impl,
                        &table.serializers,
                        true,
                    )
                    .await?;

                let row_changed = if let Some(ref old_tuple) = old_tuple_for_undo {
                    !existing_tuple_impl
                        .iter()
                        .zip(old_tuple.iter())
                        .all(|(a, b)| a == b)
                } else {
                    false
                };

                if let (Some(old_tuple), Some(serializers)) = (old_tuple_for_undo, serializers_arc)
                {
                    arena
                        .undo_log()
                        .record_append_tuple(table.table_id, old_tuple, serializers);
                }

                affected_rows += if row_changed { 2 } else { 0 };
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
