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
use crate::storage::table_codec::TableCodec;
use crate::storage::StorageTransaction;
use crate::types::index::{Index, IndexId, IndexType};
use crate::types::serialize::TupleValueSerializableImpl;
use crate::types::tuple::{TupleId, TupleImpl};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) enum UndoOp {
    RemoveTuple {
        table_id: TableId,
        tuple_id: TupleId,
    },
    AppendTuple {
        table_id: TableId,
        tuple: TupleImpl,
        serializers: Arc<Vec<TupleValueSerializableImpl>>,
        overwrite: bool,
    },
    DeleteIndex {
        table_id: TableId,
        index_id: IndexId,
        index_ty: IndexType,
        values: TupleImpl,
        tuple_id: TupleId,
    },
    AddIndex {
        table_id: TableId,
        index_id: IndexId,
        index_ty: IndexType,
        values: TupleImpl,
        tuple_id: TupleId,
    },
}

#[derive(Default)]
pub(crate) struct StatementUndoLog {
    ops: Vec<UndoOp>,
}

impl StatementUndoLog {
    pub(crate) fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub(crate) fn record_remove_tuple(&mut self, table_id: TableId, tuple_id: TupleId) {
        self.ops.push(UndoOp::RemoveTuple { table_id, tuple_id });
    }

    pub(crate) fn record_append_tuple(
        &mut self,
        table_id: TableId,
        tuple: TupleImpl,
        serializers: Arc<Vec<TupleValueSerializableImpl>>,
    ) {
        self.ops.push(UndoOp::AppendTuple {
            table_id,
            tuple,
            serializers,
            overwrite: false,
        });
    }

    pub(crate) fn record_append_tuple_overwrite(
        &mut self,
        table_id: TableId,
        tuple: TupleImpl,
        serializers: Arc<Vec<TupleValueSerializableImpl>>,
    ) {
        self.ops.push(UndoOp::AppendTuple {
            table_id,
            tuple,
            serializers,
            overwrite: true,
        });
    }

    pub(crate) fn record_delete_index(
        &mut self,
        table_id: TableId,
        index_id: IndexId,
        index_ty: IndexType,
        values: TupleImpl,
        tuple_id: TupleId,
    ) {
        self.ops.push(UndoOp::DeleteIndex {
            table_id,
            index_id,
            index_ty,
            values,
            tuple_id,
        });
    }

    pub(crate) fn record_add_index(
        &mut self,
        table_id: TableId,
        index_id: IndexId,
        index_ty: IndexType,
        values: TupleImpl,
        tuple_id: TupleId,
    ) {
        self.ops.push(UndoOp::AddIndex {
            table_id,
            index_id,
            index_ty,
            values,
            tuple_id,
        });
    }

    #[maybe_async]
    pub(crate) async fn rollback<T: StorageTransaction>(
        self,
        transaction: &mut T,
        table_codec: &mut TableCodec,
    ) -> Result<(), DatabaseError> {
        for op in self.ops.into_iter().rev() {
            match op {
                UndoOp::RemoveTuple { table_id, tuple_id } => {
                    transaction
                        .remove_tuple(table_codec, table_id, &tuple_id)
                        .await?;
                }
                UndoOp::AppendTuple {
                    table_id,
                    tuple,
                    serializers,
                    overwrite,
                } => {
                    transaction
                        .append_tuple(
                            table_codec,
                            table_id,
                            &tuple,
                            serializers.as_slice(),
                            overwrite,
                        )
                        .await?;
                }
                UndoOp::DeleteIndex {
                    table_id,
                    index_id,
                    index_ty,
                    values,
                    tuple_id,
                } => {
                    let index = Index::new(index_id, &values as &_, index_ty);
                    transaction
                        .del_index(table_codec, table_id, &index, &tuple_id)
                        .await?;
                }
                UndoOp::AddIndex {
                    table_id,
                    index_id,
                    index_ty,
                    values,
                    tuple_id,
                } => {
                    let index = Index::new(index_id, &values as &_, index_ty);
                    transaction
                        .add_index(table_codec, table_id, index, &tuple_id)
                        .await?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.ops.clear();
    }
}
