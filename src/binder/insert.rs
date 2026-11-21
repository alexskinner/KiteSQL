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

use crate::binder::Binder;
use crate::catalog::table::DmlTable;
use crate::catalog::ColumnRef;
use crate::errors::DatabaseError;
use crate::expression::ScalarExpression;
use crate::planner::operator::insert::{IdentityTarget, InsertOperator};
use crate::planner::operator::values::ValuesOperator;
use crate::planner::operator::Operator;
use crate::planner::{Childrens, ExprRef, LogicalPlan, PlanArena};
use crate::storage::StorageTransaction;
use crate::types::tuple::Schema;
use crate::types::value::DataValue;
use crate::types::LogicalType;

pub(crate) struct InsertValuesParams<'a> {
    pub table_name: &'a str,
    pub targets: Schema,
    pub rows: Vec<ExprRef>,
    pub row_count: usize,
    pub is_overwrite: bool,
    pub on_duplicate_key_update: Option<Vec<(usize, ExprRef)>>,
}

pub(crate) struct InsertQueryParams<'a> {
    pub table_name: &'a str,
    pub input_plan: LogicalPlan,
    pub targets: &'a [ColumnRef],
    pub is_overwrite: bool,
    pub on_duplicate_key_update: Option<Vec<(usize, ExprRef)>>,
}

/// The bound target table, each value column's position in it, and its identity column.
type InsertTarget = (DmlTable, Vec<Option<usize>>, Option<IdentityTarget>);

impl<T: StorageTransaction, A: AsRef<[(usize, LogicalType)]>> Binder<'_, '_, T, A> {
    fn insert_table(
        &self,
        table_name: &str,
        targets: &[ColumnRef],
        arena: &mut PlanArena<'_>,
    ) -> Result<InsertTarget, DatabaseError> {
        let catalog = self
            .context
            .table(table_name)?
            .ok_or(DatabaseError::TableNotFound)?;
        let table = catalog.dml_table(arena, |_| true)?;
        let mut sources = vec![None; table.columns.len()];
        for (position, target) in targets.iter().enumerate() {
            let column = arena.column(*target);
            if let Some((index, _)) = column.id().and_then(|id| catalog.get_column_by_id(&id)) {
                if sources[index].replace(position).is_some() {
                    return Err(DatabaseError::DuplicateTargetColumn(column.name().to_string()));
                }
            }
        }
        let identity = catalog
            .identity_column()
            .and_then(|id| catalog.get_column_by_id(&id))
            .map(|(index, _)| IdentityTarget {
                index,
                #[cfg(feature = "parser")]
                zero_generates: self.context.dialect.identity_zero_generates(),
                #[cfg(not(feature = "parser"))]
                zero_generates: false,
            });
        Ok((table, sources, identity))
    }

    pub(crate) fn bind_insert_values(
        &mut self,
        params: InsertValuesParams<'_>,
        arena: &mut PlanArena<'_>,
    ) -> Result<LogicalPlan, DatabaseError> {
        let InsertValuesParams {
            table_name,
            targets,
            rows,
            row_count,
            is_overwrite,
            on_duplicate_key_update,
        } = params;
        let (table, sources, identity) = self.insert_table(table_name, &targets, arena)?;
        let mut values = Vec::with_capacity(row_count * table.columns.len());
        for row in rows.chunks(targets.len()) {
            for (&column, source) in table.columns.iter().zip(&sources) {
                values.push(match source {
                    Some(position) => row[*position],
                    None => Self::insert_default(column, arena)?,
                });
            }
        }
        let values_plan = self.bind_values(values, row_count, table.columns.clone());

        Ok(LogicalPlan::new(
            Operator::Insert(InsertOperator {
                table,
                is_overwrite,
                on_duplicate_key_update,
                identity,
            }),
            Childrens::Only(Box::new(values_plan)),
        ))
    }

    pub(crate) fn bind_insert_query(
        &mut self,
        params: InsertQueryParams<'_>,
        arena: &mut PlanArena<'_>,
    ) -> Result<LogicalPlan, DatabaseError> {
        let InsertQueryParams {
            table_name,
            mut input_plan,
            targets,
            is_overwrite,
            on_duplicate_key_update,
        } = params;
        let (table, sources, identity) = self.insert_table(table_name, targets, arena)?;
        // Project the input to all columns in table order, filling in defaults.
        let mut projection = Vec::with_capacity(table.columns.len());
        for (&column, source) in table.columns.iter().zip(&sources) {
            projection.push(match *source {
                Some(position) => {
                    let input = input_plan.output_schema(arena)[position];
                    arena.alloc_expression(ScalarExpression::column_expr(input, position))
                }
                None => Self::insert_default(column, arena)?,
            });
        }
        let input_plan = self.bind_project(input_plan, projection, arena)?;

        Ok(LogicalPlan::new(
            Operator::Insert(InsertOperator {
                table,
                is_overwrite,
                on_duplicate_key_update,
                identity,
            }),
            Childrens::Only(Box::new(input_plan)),
        ))
    }

    fn insert_default(
        column: ColumnRef,
        arena: &mut PlanArena<'_>,
    ) -> Result<ExprRef, DatabaseError> {
        match arena.column(column).desc().default {
            Some(default) => default.clone_expression(arena),
            None => Ok(arena.alloc_expression(ScalarExpression::Constant(DataValue::Null))),
        }
    }

    pub(crate) fn bind_values(
        &mut self,
        rows: Vec<ExprRef>,
        row_count: usize,
        schema_ref: Schema,
    ) -> LogicalPlan {
        LogicalPlan::new(
            Operator::Values(ValuesOperator::new(rows, row_count, schema_ref)),
            Childrens::None,
        )
    }
}
