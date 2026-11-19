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

use super::{Binder, QueryBindStep};
use crate::catalog::ColumnRef;
use crate::errors::DatabaseError;
use crate::expression::visitor::{walk_expr, ExprVisitor};
use crate::expression::visitor_mut::{walk_mut_expr, ExprVisitorMut};
use crate::planner::MetaArena;
use crate::planner::{ExprRef, LogicalPlan, PlanArena};
use crate::storage::StorageTransaction;
use crate::types::LogicalType;
use crate::{
    expression::ScalarExpression,
    planner::operator::{aggregate::AggregateOperator, sort::SortField},
};

struct AggregateCallCollector<'a> {
    agg_calls: &'a mut Vec<ExprRef>,
}

impl ExprVisitor<dyn MetaArena + '_> for AggregateCallCollector<'_> {
    fn visit(&mut self, expr: ExprRef, arena: &(dyn MetaArena + '_)) -> Result<(), DatabaseError> {
        match arena.expression(expr) {
            ScalarExpression::AggCall { .. } => self.agg_calls.push(expr),
            ScalarExpression::Alias { expr, .. } => self.visit(*expr, arena)?,
            ScalarExpression::Empty | ScalarExpression::TableFunction(_) => unreachable!(),
            _ => walk_expr(self, expr, arena)?,
        }
        Ok(())
    }
}

/// Collects the column references of `expr`, outside aggregate calls, that belong to a source
/// whose whole primary key is grouped and are not yet GROUP BY expressions.
struct FunctionalColumnCollector<'a> {
    functional_columns: &'a [ColumnRef],
    group_by_exprs: &'a [ExprRef],
    dependent_exprs: Vec<ExprRef>,
}

impl ExprVisitor<dyn MetaArena + '_> for FunctionalColumnCollector<'_> {
    fn visit(&mut self, expr: ExprRef, arena: &(dyn MetaArena + '_)) -> Result<(), DatabaseError> {
        match arena.expression(expr) {
            ScalarExpression::AggCall { .. } => Ok(()),
            ScalarExpression::ColumnRef { column, .. } => {
                let column = *column;
                let is_functional = self
                    .functional_columns
                    .iter()
                    .any(|functional| arena.same_column(*functional, column));
                let is_new = |exprs: &[ExprRef]| {
                    !exprs.iter().any(|group_expr| {
                        group_expr
                            .unpack_alias(arena)
                            .eq_ignore_colref_pos(expr, arena)
                    })
                };
                if is_functional && is_new(self.group_by_exprs) && is_new(&self.dependent_exprs) {
                    self.dependent_exprs.push(expr);
                }
                Ok(())
            }
            ScalarExpression::Empty | ScalarExpression::TableFunction(_) => unreachable!(),
            _ => walk_expr(self, expr, arena),
        }
    }
}

impl<T: StorageTransaction, A: AsRef<[(usize, LogicalType)]>> Binder<'_, '_, T, A> {
    pub fn bind_aggregate(
        &mut self,
        children: LogicalPlan,
        agg_calls: Vec<ExprRef>,
        groupby_exprs: Vec<ExprRef>,
    ) -> Result<LogicalPlan, DatabaseError> {
        self.context.step(QueryBindStep::Agg);
        Ok(AggregateOperator::build(
            children,
            agg_calls,
            groupby_exprs,
            false,
            self.force_spill,
        ))
    }

    pub fn extract_select_aggregate(
        &mut self,
        select_items: &mut [ExprRef],
        arena: &PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        for column in select_items {
            self.collect_aggregate_calls(*column, arena)?;
        }
        Ok(())
    }

    pub fn extract_group_by_aggregate_exprs(
        &mut self,
        select_list: &mut [ExprRef],
        mut group_by_exprs: Vec<ExprRef>,
        arena: &mut PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        self.context.functional_columns =
            self.functionally_determined_columns(select_list, &group_by_exprs, arena);
        self.validate_groupby_illegal_column(select_list, &group_by_exprs, arena)?;

        for expr in group_by_exprs.iter_mut() {
            self.visit_group_by_expr(select_list, *expr, arena)?;
        }
        for expr in select_list.iter() {
            self.append_functional_group_by_exprs(*expr, arena)?;
        }
        Ok(())
    }

    /// The columns functionally dependent on the GROUP BY list (SQL:2003 feature T301): every
    /// column of a stored table source, per alias, whose primary-key columns are all grouped.
    fn functionally_determined_columns(
        &self,
        select_list: &[ExprRef],
        group_by_exprs: &[ExprRef],
        arena: &PlanArena<'_>,
    ) -> Vec<ColumnRef> {
        let grouped_columns = group_by_exprs
            .iter()
            .filter_map(|expr| {
                let expr = match arena.expression(*expr) {
                    ScalarExpression::Alias { alias, .. } => select_list
                        .iter()
                        .find(|item| {
                            matches!(
                                arena.expression(**item),
                                ScalarExpression::Alias { alias: inner, .. } if inner == alias
                            )
                        })
                        .copied()
                        .unwrap_or(*expr),
                    _ => *expr,
                };
                match arena.expression(expr.unpack_alias(arena)) {
                    ScalarExpression::ColumnRef { column, .. } => Some(*column),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let mut functional_columns = Vec::new();
        for bound_source in &self.context.bind_table[self.context.source_scope_start..] {
            if bound_source.primary_key.is_empty() {
                continue;
            }
            let key_grouped = bound_source.primary_key.iter().all(|key| {
                grouped_columns
                    .iter()
                    .any(|grouped| arena.same_column(*key, *grouped))
            });
            if key_grouped {
                functional_columns.extend_from_slice(bound_source.source.schema());
            }
        }
        functional_columns
    }

    /// Whether every column `expr` references is functionally dependent on the GROUP BY list.
    fn is_functionally_dependent(
        &self,
        expr: ExprRef,
        arena: &PlanArena<'_>,
    ) -> Result<bool, DatabaseError> {
        let functional_columns = &self.context.functional_columns;
        if functional_columns.is_empty() {
            return Ok(false);
        }
        Ok(!expr.any_referenced_column(arena, |arena, column| {
            !functional_columns
                .iter()
                .any(|functional| arena.same_column(*functional, *column))
        })?)
    }

    /// Appends the functionally dependent columns `expr` references to the GROUP BY list. They
    /// do not change the groups, and let the aggregate output them like any grouped column.
    fn append_functional_group_by_exprs(
        &mut self,
        expr: ExprRef,
        arena: &mut PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        if self.context.functional_columns.is_empty() {
            return Ok(());
        }
        let mut collector = FunctionalColumnCollector {
            functional_columns: &self.context.functional_columns,
            group_by_exprs: &self.context.group_by_exprs,
            dependent_exprs: Vec::new(),
        };
        collector.visit(expr, arena)?;
        for dependent_expr in collector.dependent_exprs {
            let group_expr = dependent_expr.clone_expression(arena)?;
            self.context.group_by_exprs.push(group_expr);
        }
        Ok(())
    }

    pub fn extract_having_orderby_aggregate_exprs<'arena, I, F>(
        &mut self,
        mut having: Option<ExprRef>,
        orderby: Option<I>,
        mut bind_sort_field: F,
        arena: &mut PlanArena<'arena>,
    ) -> Result<(Option<ExprRef>, Option<Vec<SortField>>), DatabaseError>
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item, &mut PlanArena<'arena>) -> Result<SortField, DatabaseError>,
    {
        if let Some(having) = having.as_mut() {
            self.collect_aggregate_calls(*having, arena)?;
            self.append_functional_group_by_exprs(*having, arena)?;
            self.validate_having_orderby(*having, arena)?;
        }
        let mut return_orderby = None;
        if let Some(orderby) = orderby {
            let mut fields = Vec::new();
            for orderby in orderby {
                let field = bind_sort_field(self, orderby, arena)?;
                self.collect_aggregate_calls(field.expr, arena)?;
                self.append_functional_group_by_exprs(field.expr, arena)?;
                self.validate_having_orderby(field.expr, arena)?;
                fields.push(field);
            }
            return_orderby = Some(fields);
        }
        Ok((having, return_orderby))
    }

    pub fn bind_aggregate_output_exprs<'c>(
        &mut self,
        exprs: impl IntoIterator<Item = &'c mut ExprRef>,
        arena: &mut crate::planner::PlanArena,
    ) -> Result<(), DatabaseError> {
        self.bind_aggregate_output_exprs_with_outputs(
            &self.context.agg_calls,
            &self.context.group_by_exprs,
            exprs,
            arena,
        )
    }

    pub(crate) fn bind_aggregate_output_exprs_with_outputs<'c>(
        &self,
        agg_calls: &[ExprRef],
        group_by_exprs: &[ExprRef],
        exprs: impl IntoIterator<Item = &'c mut ExprRef>,
        arena: &mut crate::planner::PlanArena,
    ) -> Result<(), DatabaseError> {
        let mut binder = AggregateOutputBinder::new(agg_calls, group_by_exprs);
        for expr in exprs {
            binder.visit(expr, arena)?;
        }
        Ok(())
    }

    pub(crate) fn collect_aggregate_calls(
        &mut self,
        expr: ExprRef,
        arena: &PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        AggregateCallCollector {
            agg_calls: &mut self.context.agg_calls,
        }
        .visit(expr, arena)
    }

    /// Validate select exprs must appear in the GROUP BY clause or be used in
    /// an aggregate function.
    /// e.g. SELECT a,count(b) FROM t GROUP BY a. it's ok.
    ///      SELECT a,b FROM t GROUP BY a.        it's error.
    ///      SELECT a,count(b) FROM t GROUP BY b. it's error.
    fn validate_groupby_illegal_column(
        &mut self,
        select_items: &[ExprRef],
        groupby: &[ExprRef],
        arena: &PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        let mut unmatched_group_exprs = Vec::with_capacity(groupby.len());
        for expr in groupby {
            if let ScalarExpression::Alias { alias, .. } = arena.expression(*expr) {
                let alias_expr = select_items.iter().find(|column| {
                    if let ScalarExpression::Alias {
                        alias: inner_alias, ..
                    } = arena.expression(**column)
                    {
                        alias == inner_alias
                    } else {
                        false
                    }
                });

                if let Some(inner_expr) = alias_expr {
                    unmatched_group_exprs.push(*inner_expr);
                }
            } else {
                unmatched_group_exprs.push(*expr);
            }
        }

        for expr in select_items {
            if expr.has_window_call(arena)? {
                HavingOrderByValidator::new(groupby, &self.context.agg_calls)
                    .visit(*expr, arena)?;
                continue;
            }
            if expr.has_agg_call(arena)? {
                continue;
            }
            if !expr.any_referenced_column(arena, |_, _| true)? {
                if let Some(position) = unmatched_group_exprs
                    .iter()
                    .position(|group_expr| expr.eq_ignore_colref_pos(*group_expr, arena))
                {
                    unmatched_group_exprs.remove(position);
                }
                continue;
            }
            let Some(position) = unmatched_group_exprs
                .iter()
                .position(|group_expr| expr.eq_ignore_colref_pos(*group_expr, arena))
            else {
                if self.is_functionally_dependent(*expr, arena)? {
                    continue;
                }
                return Err(DatabaseError::AggMiss(format!(
                    "`{}` must appear in the GROUP BY clause or be used in an aggregate function",
                    expr.output_name(arena)
                )));
            };
            unmatched_group_exprs.remove(position);
        }

        unmatched_group_exprs.retain(|expr| {
            !matches!(
                arena.expression(expr.unpack_alias(arena)),
                ScalarExpression::InitValue { .. }
            )
        });
        if !unmatched_group_exprs.is_empty() {
            return Err(DatabaseError::AggMiss(
                "in the GROUP BY clause the field must be in the select clause".to_string(),
            ));
        }

        Ok(())
    }

    fn visit_group_by_expr(
        &mut self,
        select_list: &mut [ExprRef],
        expr: ExprRef,
        arena: &mut (dyn MetaArena + '_),
    ) -> Result<(), DatabaseError> {
        if let ScalarExpression::Alias { alias, .. } = arena.expression(expr) {
            if let Some(i) = select_list.iter().position(|inner_expr| {
                if let ScalarExpression::Alias {
                    alias: inner_alias, ..
                } = arena.expression(*inner_expr)
                {
                    alias == inner_alias
                } else {
                    false
                }
            }) {
                // GROUP BY evaluates against the aggregate input, while the select
                // expression is later rewritten against aggregate output.
                self.context
                    .group_by_exprs
                    .push(select_list[i].clone_expression(arena)?);
                return Ok(());
            }
        }

        if let Some(i) = select_list
            .iter()
            .position(|column| column.eq_ignore_colref_pos(expr, arena))
        {
            self.context
                .group_by_exprs
                .push(select_list[i].clone_expression(arena)?);
        }
        Ok(())
    }

    /// Validate having or orderby clause is valid, if SQL has group by clause.
    pub fn validate_having_orderby(
        &self,
        expr: ExprRef,
        arena: &PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        if self.context.group_by_exprs.is_empty() {
            return Ok(());
        }

        Self::validate_having_orderby_with_outputs(
            expr,
            &self.context.agg_calls,
            &self.context.group_by_exprs,
            arena,
        )
    }

    pub(crate) fn validate_having_orderby_with_outputs(
        expr: ExprRef,
        agg_calls: &[ExprRef],
        group_by_exprs: &[ExprRef],
        arena: &PlanArena<'_>,
    ) -> Result<(), DatabaseError> {
        HavingOrderByValidator::new(group_by_exprs, agg_calls).visit(expr, arena)
    }
}

struct HavingOrderByValidator<'a> {
    group_by_exprs: &'a [ExprRef],
    agg_calls: &'a [ExprRef],
}

impl<'a> HavingOrderByValidator<'a> {
    fn new(group_by_exprs: &'a [ExprRef], agg_calls: &'a [ExprRef]) -> Self {
        Self {
            group_by_exprs,
            agg_calls,
        }
    }

    fn agg_miss(expr: ExprRef, arena: &dyn MetaArena) -> DatabaseError {
        DatabaseError::AggMiss(format!(
            "expression '{}' must appear in the GROUP BY clause or be used in an aggregate function",
            expr.output_name(arena)
        ))
    }
}

impl ExprVisitor<dyn MetaArena + '_> for HavingOrderByValidator<'_> {
    fn visit(&mut self, expr: ExprRef, arena: &(dyn MetaArena + '_)) -> Result<(), DatabaseError> {
        let contains = |expressions: &[ExprRef]| {
            expressions
                .iter()
                .any(|candidate| candidate.eq_ignore_colref_pos(expr, arena))
        };
        match arena.expression(expr) {
            ScalarExpression::AggCall { .. } => {
                if contains(self.group_by_exprs) || contains(self.agg_calls) {
                    Ok(())
                } else {
                    Err(Self::agg_miss(expr, arena))
                }
            }
            ScalarExpression::ColumnRef { .. } => {
                if contains(self.group_by_exprs) {
                    Ok(())
                } else {
                    Err(Self::agg_miss(expr, arena))
                }
            }
            ScalarExpression::Alias { .. } => {
                if contains(self.group_by_exprs) {
                    Ok(())
                } else {
                    self.visit(expr.unpack_alias(arena), arena)
                }
            }
            ScalarExpression::Empty | ScalarExpression::TableFunction(_) => unreachable!(),
            // A grouped expression, e.g. `ORDER BY id + 1` under `GROUP BY id + 1`.
            _ if contains(self.group_by_exprs) => Ok(()),
            _ => walk_expr(self, expr, arena),
        }
    }
}

struct AggregateOutputBinder<'a> {
    agg_calls: &'a [ExprRef],
    group_by_exprs: &'a [ExprRef],
}

impl<'a> AggregateOutputBinder<'a> {
    fn new(agg_calls: &'a [ExprRef], group_by_exprs: &'a [ExprRef]) -> Self {
        Self {
            agg_calls,
            group_by_exprs,
        }
    }

    fn output_ref<A: MetaArena + ?Sized>(
        &mut self,
        expr: ExprRef,
        arena: &mut A,
    ) -> Result<Option<ScalarExpression>, DatabaseError> {
        let output_count = self.agg_calls.len() + self.group_by_exprs.len();
        self.agg_calls
            .iter()
            .chain(self.group_by_exprs.iter())
            .position(|candidate| {
                candidate.eq_ignore_colref_pos(expr, arena)
                    || candidate
                        .unpack_alias(arena)
                        .eq_ignore_colref_pos(expr.unpack_alias(arena), arena)
            })
            .map(|position| {
                let output_expr = self
                    .agg_calls
                    .iter()
                    .chain(self.group_by_exprs.iter())
                    .nth(position)
                    .ok_or_else(|| {
                        DatabaseError::InvalidValue(format!(
                            "aggregate output position {position} is out of bounds for {output_count} output expressions"
                        ))
                    })?;
                Ok(ScalarExpression::column_expr(
                    output_expr.output_column_ref(arena),
                    position,
                ))
            })
            .transpose()
    }
}

impl<A: MetaArena + ?Sized> ExprVisitorMut<A> for AggregateOutputBinder<'_> {
    fn visit(&mut self, expr: &mut ExprRef, arena: &mut A) -> Result<(), DatabaseError> {
        if let ScalarExpression::Alias {
            alias: crate::expression::AliasType::Name(_),
            ..
        } = arena.expression(*expr)
        {
            return walk_mut_expr(self, expr, arena);
        }

        if let Some(output) = self.output_ref(*expr, arena)? {
            *expr = arena.alloc_expression(output);
            return Ok(());
        }

        walk_mut_expr(self, expr, arena)
    }
}

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
mod tests {
    use super::AggregateOutputBinder;
    use crate::binder::test::build_t1_table;
    use crate::binder::{Binder, BinderContext};
    use crate::catalog::{ColumnCatalog, ColumnDesc, ColumnRef};
    use crate::db::DataBaseBuilder;
    use crate::errors::DatabaseError;
    use crate::expression::agg::AggKind;
    use crate::expression::visitor_mut::ExprVisitorMut;
    use crate::expression::{AliasType, BinaryOperator, ScalarExpression};
    use crate::planner::operator::Operator;
    use crate::planner::{ExprRef, LogicalPlan, PlanArena};
    use crate::storage::Storage;
    use crate::types::LogicalType;
    use tempfile::TempDir;

    fn test_column(arena: &mut PlanArena, name: &str, ty: LogicalType) -> ColumnRef {
        arena.alloc_column(ColumnCatalog::new(
            name.to_string(),
            true,
            ColumnDesc::new(ty, None, false, None).unwrap(),
        ))
    }

    fn test_count(arena: &mut PlanArena, expr: ExprRef) -> ExprRef {
        arena.alloc_expression(ScalarExpression::AggCall {
            distinct: false,
            kind: AggKind::Count,
            args: vec![expr],
            ty: LogicalType::Bigint,
        })
    }

    #[test]
    fn test_aggregate_output_binder_rewrites_agg_and_group_slots() -> Result<(), DatabaseError> {
        let table_arena = crate::planner::TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);
        let group_column = test_column(&mut arena, "c1", LogicalType::Integer);
        let agg_column = test_column(&mut arena, "c2", LogicalType::Integer);

        let group_expr = arena.alloc_expression(ScalarExpression::column_expr(group_column, 0));
        let agg_arg = arena.alloc_expression(ScalarExpression::column_expr(agg_column, 1));
        let agg_expr = test_count(&mut arena, agg_arg);

        let agg_output = arena.alloc_expression(ScalarExpression::Alias {
            expr: agg_expr,
            alias: AliasType::Name("cnt".to_string()),
        });
        let group_output = arena.alloc_expression(ScalarExpression::Alias {
            expr: group_expr,
            alias: AliasType::Name("g".to_string()),
        });

        let mut order_by_agg = arena.alloc_expression(ScalarExpression::Alias {
            expr: agg_expr,
            alias: AliasType::Name("cnt".to_string()),
        });
        let mut order_by_group = group_expr;
        {
            let mut binder = AggregateOutputBinder::new(
                std::slice::from_ref(&agg_output),
                std::slice::from_ref(&group_output),
            );
            binder.visit(&mut order_by_agg, &mut arena)?;
            binder.visit(&mut order_by_group, &mut arena)?;
        }
        let agg_column = agg_output.output_column_ref(&mut arena);
        let expected_agg_inner =
            arena.alloc_expression(ScalarExpression::column_expr(agg_column, 0));
        let expected_agg = arena.alloc_expression(ScalarExpression::Alias {
            expr: expected_agg_inner,
            alias: AliasType::Name("cnt".to_string()),
        });
        assert!(order_by_agg.eq_ignore_colref_pos(expected_agg, &arena));

        let group_column = group_output.output_column_ref(&mut arena);
        let expected_group = arena.alloc_expression(ScalarExpression::column_expr(group_column, 1));
        assert!(order_by_group.eq_ignore_colref_pos(expected_group, &arena));

        Ok(())
    }

    #[test]
    fn test_aggregate_output_binder_matches_alias_expr_reference() -> Result<(), DatabaseError> {
        let table_arena = crate::planner::TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);
        let group_column = test_column(&mut arena, "c1", LogicalType::Integer);
        let group_expr = arena.alloc_expression(ScalarExpression::column_expr(group_column, 0));
        let group_output = arena.alloc_expression(ScalarExpression::Alias {
            expr: group_expr,
            alias: AliasType::Name("g".to_string()),
        });

        let identity = test_column(&mut arena, "identity", LogicalType::Integer);
        let mut target = arena.alloc_expression(ScalarExpression::Alias {
            expr: group_expr,
            alias: AliasType::Column(identity),
        });

        {
            let mut binder = AggregateOutputBinder::new(&[], std::slice::from_ref(&group_output));
            binder.visit(&mut target, &mut arena)?;
        }
        let output_column = group_output.output_column_ref(&mut arena);
        let expected = arena.alloc_expression(ScalarExpression::column_expr(output_column, 0));
        assert!(target.eq_ignore_colref_pos(expected, &arena));

        Ok(())
    }

    fn aggregate_group_by_len(plan: &LogicalPlan) -> Option<usize> {
        if let Operator::Aggregate(aggregate) = &plan.operator {
            return Some(aggregate.groupby_exprs.len());
        }
        plan.childrens.iter().find_map(aggregate_group_by_len)
    }

    #[test]
    async fn test_functional_dependency_on_primary_key() -> Result<(), DatabaseError> {
        let tables = build_t1_table().await?;

        // `c2` is appended to the GROUP BY list because it depends on the key `c1`.
        let plan = tables.plan("select c1, c2 from t1 group by c1")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        let plan = tables.plan("select c1, count(*) from t1 group by c1 having c2 > 1")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        let plan = tables.plan("select c1 from t1 group by c1 order by c2 + 1")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        // Repeated references append the column once.
        let plan = tables.plan("select c1, c2, c2 + 1 from t1 group by c1 order by c2")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        let plan = tables.plan("select t.c1, t.c2 from t1 t group by t.c1 order by t.c2")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        let plan = tables.plan("select c1 as k, c2 from t1 group by k")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));

        // A grouped expression stays usable as a whole in ORDER BY and HAVING.
        tables.plan(
            "select c2 + 1, count(*) from t1 group by c2 + 1 having c2 + 1 > 1 order by c2 + 1",
        )?;

        // Grouping a non-key column determines nothing else.
        assert!(matches!(
            tables.plan("select c2, c1 from t1 group by c2"),
            Err(DatabaseError::AggMiss(_))
        ));
        assert!(matches!(
            tables.plan("select c2 from t1 group by c2 order by c1"),
            Err(DatabaseError::AggMiss(_))
        ));
        assert!(matches!(
            tables.plan("select c2, count(*) from t1 group by c2 having c1 > 1"),
            Err(DatabaseError::AggMiss(_))
        ));
        // An expression over the key is not the key.
        assert!(matches!(
            tables.plan("select c1 + 1 as k, c2 from t1 group by k"),
            Err(DatabaseError::AggMiss(_))
        ));
        // A key only determines its own source.
        assert!(matches!(
            tables.plan("select c1, c4 from t1 join t2 on c1 = c3 group by c1"),
            Err(DatabaseError::AggMiss(_))
        ));
        let plan =
            tables.plan("select c1, c2, c3, c4 from t1 join t2 on c1 = c3 group by c1, c3")?;
        assert_eq!(aggregate_group_by_len(&plan), Some(4));
        // Derived tables carry no key.
        assert!(matches!(
            tables.plan("select d.c1, d.c2 from (select c1, c2 from t1) d group by d.c1"),
            Err(DatabaseError::AggMiss(_))
        ));

        Ok(())
    }

    #[test]
    async fn test_functional_dependency_per_alias_in_self_join() -> Result<(), DatabaseError> {
        let tables = build_t1_table().await?;

        let plan = tables.plan(
            "select p.c1, p.c2, count(c.c1) from t1 p left join t1 c on c.c2 = p.c1 \
             group by p.c1 order by p.c2",
        )?;
        assert_eq!(aggregate_group_by_len(&plan), Some(2));
        for sql in [
            "select p.c1, c.c2 from t1 p left join t1 c on c.c2 = p.c1 group by p.c1",
            "select p.c1, count(*) from t1 p left join t1 c on c.c2 = p.c1 group by p.c1 \
             order by c.c2",
            "select p.c1, count(*) from t1 p left join t1 c on c.c2 = p.c1 group by p.c1 \
             having c.c2 > 1",
        ] {
            assert!(
                matches!(tables.plan(sql), Err(DatabaseError::AggMiss(_))),
                "{sql}"
            );
        }
        let plan = tables.plan(
            "select p.c1, c.c1, p.c2, c.c2 from t1 p join t1 c on c.c2 = p.c1 \
             group by p.c1, c.c1",
        )?;
        assert_eq!(aggregate_group_by_len(&plan), Some(4));

        Ok(())
    }

    #[test]
    async fn test_functional_dependency_on_composite_key() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");
        let mut database = DataBaseBuilder::path(temp_dir.path())
            .build_in_memory()
            .await?;
        database
            .ddl("create table fd_pair (a int, b int, v varchar, primary key (a, b))")
            .await?;
        database
            .run("insert into fd_pair values (1, 1, 'x'), (1, 2, 'y'), (2, 1, 'z')")
            .await?
            .done()
            .await?;

        let mut iter = database
            .run("select k.b, k.a, k.v from fd_pair k group by k.b, k.a having k.v <> 'y' order by k.v desc")
            .await?;
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                (0..tuple.len())
                    .map(|i| tuple.get(i).map(ToString::to_string).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .await?
        {
            rows.push(row);
        }
        iter.done().await?;
        assert_eq!(rows, vec!["1 2 z".to_string(), "1 1 x".to_string()]);

        assert!(matches!(
            database.run("select a, v from fd_pair group by a").await,
            Err(DatabaseError::AggMiss(_))
        ));

        Ok(())
    }

    #[test]
    async fn test_validate_having_orderby_rejects_missing_group_expr() -> Result<(), DatabaseError>
    {
        let tables = build_t1_table().await?;
        let function_registry = crate::function::registry::FunctionRegistry::new();
        let transaction = tables.storage.transaction()?;
        let arena = tables.table_arena.borrow();
        let args: [(usize, LogicalType); 0] = [];
        #[cfg(feature = "parser")]
        let generic_dialect = crate::dialect::GenericSqlDialect::new();
        let mut binder = Binder::new(
            BinderContext::new(
                &arena.names,
                &arena.tables,
                &arena.views,
                &transaction,
                &function_registry,
                #[cfg(feature = "parser")]
                &generic_dialect,
            ),
            &args,
            None,
        );
        let table_arena = crate::planner::TableArenaCell::default();
        let mut plan_arena = PlanArena::new(&table_arena);
        let group_column = test_column(&mut plan_arena, "c1", LogicalType::Integer);
        let missing_column = test_column(&mut plan_arena, "c2", LogicalType::Integer);
        let group_expr =
            plan_arena.alloc_expression(ScalarExpression::column_expr(group_column, 0));
        let missing_expr =
            plan_arena.alloc_expression(ScalarExpression::column_expr(missing_column, 1));
        binder.context.group_by_exprs.push(group_expr);

        binder.validate_having_orderby(group_expr, &plan_arena)?;
        let group_alias = plan_arena.alloc_expression(ScalarExpression::Alias {
            expr: group_expr,
            alias: AliasType::Name("group_alias".to_string()),
        });
        binder.validate_having_orderby(group_alias, &plan_arena)?;

        assert!(matches!(
            binder.validate_having_orderby(missing_expr, &plan_arena),
            Err(DatabaseError::AggMiss(_))
        ));

        let registered_agg = test_count(&mut plan_arena, missing_expr);
        binder.context.agg_calls.push(registered_agg);
        binder.validate_having_orderby(registered_agg, &plan_arena)?;
        let constant = plan_arena.alloc_expression(ScalarExpression::Constant(1_i32.into()));
        let unregistered_agg = test_count(&mut plan_arena, constant);
        assert!(matches!(
            binder.validate_having_orderby(unregistered_agg, &plan_arena),
            Err(DatabaseError::AggMiss(_))
        ));

        let invalid_binary = plan_arena.alloc_expression(ScalarExpression::Binary {
            op: BinaryOperator::Eq,
            left_expr: group_expr,
            right_expr: missing_expr,
            evaluator: None,
            ty: LogicalType::Boolean,
        });
        assert!(matches!(
            binder.validate_having_orderby(invalid_binary, &plan_arena),
            Err(DatabaseError::AggMiss(_))
        ));

        Ok(())
    }
}
// GRCOV_EXCL_STOP
