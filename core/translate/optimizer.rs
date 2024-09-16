use std::{collections::HashMap, rc::Rc};

use sqlite3_parser::ast;

use crate::{schema::BTreeTable, util::normalize_ident, Result};

use super::plan::{
    get_table_ref_bitmask_for_ast_expr, get_table_ref_bitmask_for_operator, Operator, Plan,
    ProjectionColumn,
};

/**
 * Make a few passes over the plan to optimize it.
 */
pub fn optimize_plan(mut select_plan: Plan) -> Result<(Plan, ExpressionResultCache)> {
    let mut expr_result_cache = ExpressionResultCache::new();
    push_predicates(
        &mut select_plan.root_operator,
        &select_plan.referenced_tables,
    )?;
    if eliminate_constants(&mut select_plan.root_operator)?
        == ConstantConditionEliminationResult::ImpossibleCondition
    {
        return Ok((
            Plan {
                root_operator: Operator::Nothing,
                referenced_tables: vec![],
            },
            expr_result_cache,
        ));
    }
    use_indexes(
        &mut select_plan.root_operator,
        &select_plan.referenced_tables,
    )?;
    find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(&select_plan.root_operator, &mut expr_result_cache);
    Ok((select_plan, expr_result_cache))
}

/**
 * Use indexes where possible (currently just primary key lookups)
 */
fn use_indexes(
    operator: &mut Operator,
    referenced_tables: &[(Rc<BTreeTable>, String)],
) -> Result<()> {
    match operator {
        Operator::Scan {
            table,
            predicates: filter,
            table_identifier,
            id,
            ..
        } => {
            if filter.is_none() {
                return Ok(());
            }

            let fs = filter.as_mut().unwrap();
            let mut i = 0;
            let mut maybe_rowid_predicate = None;
            while i < fs.len() {
                let f = fs[i].take_ownership();
                let table_index = referenced_tables
                    .iter()
                    .position(|(t, t_id)| Rc::ptr_eq(t, table) && t_id == table_identifier)
                    .unwrap();
                let (can_use, expr) =
                    try_extract_rowid_comparison_expression(f, table_index, referenced_tables)?;
                if can_use {
                    maybe_rowid_predicate = Some(expr);
                    fs.remove(i);
                    break;
                } else {
                    fs[i] = expr;
                    i += 1;
                }
            }

            if let Some(rowid_predicate) = maybe_rowid_predicate {
                let predicates_owned = if fs.is_empty() {
                    None
                } else {
                    Some(std::mem::take(fs))
                };
                *operator = Operator::SeekRowid {
                    table: table.clone(),
                    table_identifier: table_identifier.clone(),
                    rowid_predicate,
                    predicates: predicates_owned,
                    id: *id,
                    step: 0,
                }
            }

            Ok(())
        }
        Operator::Aggregate { source, .. } => {
            use_indexes(source, referenced_tables)?;
            Ok(())
        }
        Operator::Filter { source, .. } => {
            use_indexes(source, referenced_tables)?;
            Ok(())
        }
        Operator::SeekRowid { .. } => Ok(()),
        Operator::Limit { source, .. } => {
            use_indexes(source, referenced_tables)?;
            Ok(())
        }
        Operator::Join { left, right, .. } => {
            use_indexes(left, referenced_tables)?;
            use_indexes(right, referenced_tables)?;
            Ok(())
        }
        Operator::Order { source, .. } => {
            use_indexes(source, referenced_tables)?;
            Ok(())
        }
        Operator::Projection { source, .. } => {
            use_indexes(source, referenced_tables)?;
            Ok(())
        }
        Operator::Nothing => Ok(()),
    }
}

#[derive(Debug, PartialEq, Clone)]
enum ConstantConditionEliminationResult {
    Continue,
    ImpossibleCondition,
}

// removes predicates that are always true
// returns a ConstantEliminationResult indicating whether any predicates are always false
fn eliminate_constants(operator: &mut Operator) -> Result<ConstantConditionEliminationResult> {
    match operator {
        Operator::Filter {
            source, predicates, ..
        } => {
            let mut i = 0;
            while i < predicates.len() {
                let predicate = &predicates[i];
                if predicate.is_always_true()? {
                    predicates.remove(i);
                } else if predicate.is_always_false()? {
                    return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
                } else {
                    i += 1;
                }
            }

            if predicates.is_empty() {
                *operator = source.take_ownership();
                eliminate_constants(operator)?;
            } else {
                eliminate_constants(source)?;
            }

            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Join {
            left,
            right,
            predicates,
            outer,
            ..
        } => {
            if eliminate_constants(left)? == ConstantConditionEliminationResult::ImpossibleCondition
            {
                return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
            }
            if eliminate_constants(right)?
                == ConstantConditionEliminationResult::ImpossibleCondition
                && !*outer
            {
                return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
            }

            if predicates.is_none() {
                return Ok(ConstantConditionEliminationResult::Continue);
            }

            let predicates = predicates.as_mut().unwrap();

            let mut i = 0;
            while i < predicates.len() {
                let predicate = &predicates[i];
                if predicate.is_always_true()? {
                    predicates.remove(i);
                } else if predicate.is_always_false()? && !*outer {
                    return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
                } else {
                    i += 1;
                }
            }

            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Aggregate { source, .. } => {
            if eliminate_constants(source)?
                == ConstantConditionEliminationResult::ImpossibleCondition
            {
                *source = Box::new(Operator::Nothing);
            }
            // Aggregation operator can return a row even if the source is empty e.g. count(1) from users where 0
            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::SeekRowid {
            rowid_predicate,
            predicates,
            ..
        } => {
            if let Some(predicates) = predicates {
                let mut i = 0;
                while i < predicates.len() {
                    let predicate = &predicates[i];
                    if predicate.is_always_true()? {
                        predicates.remove(i);
                    } else if predicate.is_always_false()? {
                        return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
                    } else {
                        i += 1;
                    }
                }
            }

            if rowid_predicate.is_always_false()? {
                return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
            }

            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Limit { source, .. } => {
            let constant_elimination_result = eliminate_constants(source)?;
            if constant_elimination_result
                == ConstantConditionEliminationResult::ImpossibleCondition
            {
                *operator = Operator::Nothing;
            }
            Ok(constant_elimination_result)
        }
        Operator::Order { source, .. } => {
            if eliminate_constants(source)?
                == ConstantConditionEliminationResult::ImpossibleCondition
            {
                *operator = Operator::Nothing;
                return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
            }
            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Projection { source, .. } => {
            if eliminate_constants(source)?
                == ConstantConditionEliminationResult::ImpossibleCondition
            {
                *operator = Operator::Nothing;
                return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
            }

            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Scan { predicates, .. } => {
            if let Some(ps) = predicates {
                let mut i = 0;
                while i < ps.len() {
                    let predicate = &ps[i];
                    if predicate.is_always_true()? {
                        ps.remove(i);
                    } else if predicate.is_always_false()? {
                        return Ok(ConstantConditionEliminationResult::ImpossibleCondition);
                    } else {
                        i += 1;
                    }
                }

                if ps.is_empty() {
                    *predicates = None;
                }
            }
            Ok(ConstantConditionEliminationResult::Continue)
        }
        Operator::Nothing => Ok(ConstantConditionEliminationResult::Continue),
    }
}

/**
  Recursively pushes predicates down the tree, as far as possible.
*/
fn push_predicates(
    operator: &mut Operator,
    referenced_tables: &Vec<(Rc<BTreeTable>, String)>,
) -> Result<()> {
    match operator {
        Operator::Filter {
            source, predicates, ..
        } => {
            let mut i = 0;
            while i < predicates.len() {
                // try to push the predicate to the source
                // if it succeeds, remove the predicate from the filter
                let predicate_owned = predicates[i].take_ownership();
                let Some(predicate) = push_predicate(source, predicate_owned, referenced_tables)?
                else {
                    predicates.remove(i);
                    continue;
                };
                predicates[i] = predicate;
                i += 1;
            }

            if predicates.is_empty() {
                *operator = source.take_ownership();
            }

            Ok(())
        }
        Operator::Join {
            left,
            right,
            predicates,
            outer,
            ..
        } => {
            push_predicates(left, referenced_tables)?;
            push_predicates(right, referenced_tables)?;

            if predicates.is_none() {
                return Ok(());
            }

            let predicates = predicates.as_mut().unwrap();

            let mut i = 0;
            while i < predicates.len() {
                // try to push the predicate to the left side first, then to the right side

                // temporarily take ownership of the predicate
                let predicate_owned = predicates[i].take_ownership();
                // left join predicates cant be pushed to the left side
                let push_result = if *outer {
                    Some(predicate_owned)
                } else {
                    push_predicate(left, predicate_owned, referenced_tables)?
                };
                // if the predicate was pushed to a child, remove it from the list
                let Some(predicate) = push_result else {
                    predicates.remove(i);
                    continue;
                };
                // otherwise try to push it to the right side
                // if it was pushed to the right side, remove it from the list
                let Some(predicate) = push_predicate(right, predicate, referenced_tables)? else {
                    predicates.remove(i);
                    continue;
                };
                // otherwise keep the predicate in the list
                predicates[i] = predicate;
                i += 1;
            }

            Ok(())
        }
        Operator::Aggregate { source, .. } => {
            push_predicates(source, referenced_tables)?;

            Ok(())
        }
        Operator::SeekRowid { .. } => Ok(()),
        Operator::Limit { source, .. } => {
            push_predicates(source, referenced_tables)?;
            Ok(())
        }
        Operator::Order { source, .. } => {
            push_predicates(source, referenced_tables)?;
            Ok(())
        }
        Operator::Projection { source, .. } => {
            push_predicates(source, referenced_tables)?;
            Ok(())
        }
        Operator::Scan { .. } => Ok(()),
        Operator::Nothing => Ok(()),
    }
}

/**
  Push a single predicate down the tree, as far as possible.
  Returns Ok(None) if the predicate was pushed, otherwise returns itself as Ok(Some(predicate))
*/
fn push_predicate(
    operator: &mut Operator,
    predicate: ast::Expr,
    referenced_tables: &Vec<(Rc<BTreeTable>, String)>,
) -> Result<Option<ast::Expr>> {
    match operator {
        Operator::Scan {
            predicates,
            table_identifier,
            ..
        } => {
            let table_index = referenced_tables
                .iter()
                .position(|(_, t_id)| t_id == table_identifier)
                .unwrap();

            let predicate_bitmask =
                get_table_ref_bitmask_for_ast_expr(referenced_tables, &predicate)?;

            // the expression is allowed to refer to tables on its left, i.e. the righter bits in the mask
            // e.g. if this table is 0010, and the table on its right in the join is 0100:
            // if predicate_bitmask is 0011, the predicate can be pushed (refers to this table and the table on its left)
            // if predicate_bitmask is 0001, the predicate can be pushed (refers to the table on its left)
            // if predicate_bitmask is 0101, the predicate can't be pushed (refers to this table and a table on its right)
            let next_table_on_the_right_in_join_bitmask = 1 << (table_index + 1);
            if predicate_bitmask >= next_table_on_the_right_in_join_bitmask {
                return Ok(Some(predicate));
            }

            if predicates.is_none() {
                predicates.replace(vec![predicate]);
            } else {
                predicates.as_mut().unwrap().push(predicate);
            }

            Ok(None)
        }
        Operator::Filter {
            source,
            predicates: ps,
            ..
        } => {
            let push_result = push_predicate(source, predicate, referenced_tables)?;
            if push_result.is_none() {
                return Ok(None);
            }

            ps.push(push_result.unwrap());

            Ok(None)
        }
        Operator::Join {
            left,
            right,
            predicates: join_on_preds,
            outer,
            ..
        } => {
            let push_result_left = push_predicate(left, predicate, referenced_tables)?;
            if push_result_left.is_none() {
                return Ok(None);
            }
            let push_result_right =
                push_predicate(right, push_result_left.unwrap(), referenced_tables)?;
            if push_result_right.is_none() {
                return Ok(None);
            }

            if *outer {
                return Ok(Some(push_result_right.unwrap()));
            }

            let pred = push_result_right.unwrap();

            let table_refs_bitmask = get_table_ref_bitmask_for_ast_expr(referenced_tables, &pred)?;

            let left_bitmask = get_table_ref_bitmask_for_operator(referenced_tables, left)?;
            let right_bitmask = get_table_ref_bitmask_for_operator(referenced_tables, right)?;

            if table_refs_bitmask & left_bitmask == 0 || table_refs_bitmask & right_bitmask == 0 {
                return Ok(Some(pred));
            }

            if join_on_preds.is_none() {
                join_on_preds.replace(vec![pred]);
            } else {
                join_on_preds.as_mut().unwrap().push(pred);
            }

            Ok(None)
        }
        Operator::Aggregate { source, .. } => {
            let push_result = push_predicate(source, predicate, referenced_tables)?;
            if push_result.is_none() {
                return Ok(None);
            }

            Ok(Some(push_result.unwrap()))
        }
        Operator::SeekRowid { .. } => Ok(Some(predicate)),
        Operator::Limit { source, .. } => {
            let push_result = push_predicate(source, predicate, referenced_tables)?;
            if push_result.is_none() {
                return Ok(None);
            }

            Ok(Some(push_result.unwrap()))
        }
        Operator::Order { source, .. } => {
            let push_result = push_predicate(source, predicate, referenced_tables)?;
            if push_result.is_none() {
                return Ok(None);
            }

            Ok(Some(push_result.unwrap()))
        }
        Operator::Projection { source, .. } => {
            let push_result = push_predicate(source, predicate, referenced_tables)?;
            if push_result.is_none() {
                return Ok(None);
            }

            Ok(Some(push_result.unwrap()))
        }
        Operator::Nothing => Ok(Some(predicate)),
    }
}

#[derive(Debug)]
pub struct ExpressionResultCache {
    resultmap: HashMap<usize, CachedResult>,
    keymap: HashMap<usize, Vec<usize>>,
}

#[derive(Debug)]
pub struct CachedResult {
    pub register_idx: usize,
    pub source_expr: ast::Expr,
}

const OPERATOR_ID_MULTIPLIER: usize = 10000;

/**
  ExpressionResultCache is a cache for the results of expressions that are computed in the query plan,
  or more precisely, the VM registers that hold the results of these expressions.

  Right now the cache is mainly used to avoid recomputing e.g. the result of an aggregation expression
  e.g. SELECT t.a, SUM(t.b) FROM t GROUP BY t.a ORDER BY SUM(t.b)
*/
impl ExpressionResultCache {
    pub fn new() -> Self {
        ExpressionResultCache {
            resultmap: HashMap::new(),
            keymap: HashMap::new(),
        }
    }

    /**
        Store the result of an expression that is computed in the query plan.
        The result is stored in a VM register. A copy of the expression AST node is
        stored as well, so that parent operators can use it to compare their own expressions
        with the one that was computed in a child operator.

        This is a weakness of our current reliance on a 3rd party AST library, as we can't
        e.g. modify the AST to add identifiers to nodes or replace nodes with some kind of
        reference to a register, etc.
    */
    pub fn cache_result_register(
        &mut self,
        operator_id: usize,
        result_column_idx: usize,
        register_idx: usize,
        expr: ast::Expr,
    ) {
        let key = operator_id * OPERATOR_ID_MULTIPLIER + result_column_idx;
        self.resultmap.insert(
            key,
            CachedResult {
                register_idx,
                source_expr: expr,
            },
        );
    }

    /**
      Set a mapping from a parent operator to a child operator, so that the parent operator
      can look up the register of a result that was computed in the child operator.
      E.g. "Parent operator's result column 3 is computed in child operator 5, result column 2"
    */
    pub fn set_precomputation_key(
        &mut self,
        operator_id: usize,
        result_column_idx: usize,
        child_operator_id: usize,
        child_operator_result_column_idx_mask: usize,
    ) {
        let key = operator_id * OPERATOR_ID_MULTIPLIER + result_column_idx;

        let mut values = Vec::new();
        for i in 0..64 {
            if (child_operator_result_column_idx_mask >> i) & 1 == 1 {
                values.push(child_operator_id * OPERATOR_ID_MULTIPLIER + i);
            }
        }
        self.keymap.insert(key, values);
    }

    /**
      Get the cache entries for a given operator and result column index.
      There may be multiple cached entries, e.g. a binary operator's both
      arms may have been cached.
    */
    pub fn get_cached_result_registers(
        &self,
        operator_id: usize,
        result_column_idx: usize,
    ) -> Option<Vec<&CachedResult>> {
        let key = operator_id * OPERATOR_ID_MULTIPLIER + result_column_idx;
        self.keymap.get(&key).and_then(|keys| {
            let mut results = Vec::new();
            for key in keys {
                if let Some(result) = self.resultmap.get(key) {
                    results.push(result);
                }
            }
            if results.is_empty() {
                None
            } else {
                Some(results)
            }
        })
    }
}

type ResultColumnIndexBitmask = usize;

/**
  Find all result columns in an operator that match an expression, either fully or partially.
  This is used to find the result columns that are computed in an operator and that are used
  in a parent operator, so that the parent operator can look up the register that holds the result
  of the child operator's expression.

  The result is returned as a bitmask due to performance neuroticism. A limitation of this is that
  we can only handle 64 result columns per operator.
*/
fn find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
    expr: &ast::Expr,
    operator: &Operator,
) -> ResultColumnIndexBitmask {
    let exact_match = match operator {
        Operator::Aggregate {
            aggregates,
            group_by,
            ..
        } => {
            let mut idx = 0;
            let mut mask = 0;
            for agg in aggregates.iter() {
                if agg.original_expr == *expr {
                    mask |= 1 << idx;
                }
                idx += 1;
            }

            if let Some(group_by) = group_by {
                for g in group_by.iter() {
                    if g == expr {
                        mask |= 1 << idx;
                    }
                    idx += 1
                }
            }

            mask
        }
        Operator::Filter { .. } => 0,
        Operator::SeekRowid { .. } => 0,
        Operator::Limit { .. } => 0,
        Operator::Join { .. } => 0,
        Operator::Order { .. } => 0,
        Operator::Projection { expressions, .. } => {
            let mut idx = 0;
            let mut mask = 0;
            for e in expressions.iter() {
                match e {
                    super::plan::ProjectionColumn::Column(c) => {
                        if c == expr {
                            mask |= 1 << idx;
                        }
                    }
                    super::plan::ProjectionColumn::Star => {}
                    super::plan::ProjectionColumn::TableStar(_, _) => {}
                }
                idx += 1;
            }

            mask
        }
        Operator::Scan { .. } => 0,
        Operator::Nothing => 0,
    };

    if exact_match != 0 {
        return exact_match;
    }

    match expr {
        ast::Expr::Between {
            lhs,
            not,
            start,
            end,
        } => {
            let mut mask = 0;
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(lhs, operator);
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(start, operator);
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(end, operator);
            mask
        }
        ast::Expr::Binary(lhs, op, rhs) => {
            let mut mask = 0;
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(lhs, operator);
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(rhs, operator);
            mask
        }
        ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            let mut mask = 0;
            if let Some(base) = base {
                mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(base, operator);
            }
            for (w, t) in when_then_pairs.iter() {
                mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(w, operator);
                mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(t, operator);
            }
            if let Some(e) = else_expr {
                mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(e, operator);
            }
            mask
        }
        ast::Expr::Cast { expr, type_name } => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                expr, operator,
            )
        }
        ast::Expr::Collate(expr, collation) => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                expr, operator,
            )
        }
        ast::Expr::DoublyQualified(schema, tbl, ident) => 0,
        ast::Expr::Exists(_) => 0,
        ast::Expr::FunctionCall {
            name,
            distinctness,
            args,
            order_by,
            filter_over,
        } => {
            let mut mask = 0;
            if let Some(args) = args {
                for a in args.iter() {
                    mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(a, operator);
                }
            }
            mask
        }
        ast::Expr::FunctionCallStar { name, filter_over } => 0,
        ast::Expr::Id(_) => 0,
        ast::Expr::InList { lhs, not, rhs } => {
            let mut mask = 0;
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(lhs, operator);
            if let Some(rhs) = rhs {
                for r in rhs.iter() {
                    mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(r, operator);
                }
            }
            mask
        }
        ast::Expr::InSelect { lhs, not, rhs } => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                lhs, operator,
            )
        }
        ast::Expr::InTable {
            lhs,
            not,
            rhs,
            args,
        } => 0,
        ast::Expr::IsNull(expr) => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                expr, operator,
            )
        }
        ast::Expr::Like {
            lhs,
            not,
            op,
            rhs,
            escape,
        } => {
            let mut mask = 0;
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(lhs, operator);
            mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(rhs, operator);
            mask
        }
        ast::Expr::Literal(_) => 0,
        ast::Expr::Name(_) => 0,
        ast::Expr::NotNull(expr) => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                expr, operator,
            )
        }
        ast::Expr::Parenthesized(expr) => {
            let mut mask = 0;
            for e in expr.iter() {
                mask |= find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(e, operator);
            }
            mask
        }
        ast::Expr::Qualified(_, _) => 0,
        ast::Expr::Raise(_, _) => 0,
        ast::Expr::Subquery(_) => 0,
        ast::Expr::Unary(op, expr) => {
            find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(
                expr, operator,
            )
        }
        ast::Expr::Variable(_) => 0,
    }
}

/**
 * This function is used to find all the expressions that are shared between the parent operator and the child operators.
 * If an expression is shared between the parent and child operators, then the parent operator should not recompute the expression.
 * Instead, it should use the result of the expression that was computed by the child operator.
*/
fn find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(
    operator: &Operator,
    expr_result_cache: &mut ExpressionResultCache,
) {
    match operator {
        Operator::Aggregate {
            source,
            ..
        } => {
            find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(
                source, expr_result_cache,
            )
        }
        Operator::Filter { .. } => unreachable!(),
        Operator::SeekRowid { .. } => {}
        Operator::Limit { source, .. } => {
            find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(source, expr_result_cache)
        }
        Operator::Join { .. } => {}
        Operator::Order { source, key, .. } => {
            let mut idx = 0;

            for (expr, _) in key.iter() {
                let result = find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(expr, source);
                if result != 0 {
                    expr_result_cache.set_precomputation_key(
                        operator.id(),
                        idx,
                        source.id(),
                        result,
                    );
                }
                idx += 1;
            }
            find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(source, expr_result_cache)
        }
        Operator::Projection { source, expressions, .. } => {
            let mut idx = 0;
            for expr in expressions.iter() {
                if let ProjectionColumn::Column(expr) = expr {
                    let result = find_indexes_of_all_result_columns_in_operator_that_match_expr_either_fully_or_partially(expr, source);
                    if result != 0 {
                        expr_result_cache.set_precomputation_key(
                            operator.id(),
                            idx,
                            source.id(),
                            result,
                        );
                    }
                }
                idx += 1;
            }
            find_shared_expressions_in_child_operators_and_mark_them_so_that_the_parent_operator_doesnt_recompute_them(source, expr_result_cache)
        }
        Operator::Scan { .. } => {}
        Operator::Nothing => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstantPredicate {
    AlwaysTrue,
    AlwaysFalse,
}

/**
  Helper trait for expressions that can be optimized
  Implemented for ast::Expr
*/
pub trait Optimizable {
    // if the expression is a constant expression e.g. '1', returns the constant condition
    fn check_constant(&self) -> Result<Option<ConstantPredicate>>;
    fn is_always_true(&self) -> Result<bool> {
        Ok(self
            .check_constant()?
            .map_or(false, |c| c == ConstantPredicate::AlwaysTrue))
    }
    fn is_always_false(&self) -> Result<bool> {
        Ok(self
            .check_constant()?
            .map_or(false, |c| c == ConstantPredicate::AlwaysFalse))
    }
    // if the expression is the primary key of a table, returns the index of the table
    fn check_primary_key(
        &self,
        referenced_tables: &[(Rc<BTreeTable>, String)],
    ) -> Result<Option<usize>>;
}

impl Optimizable for ast::Expr {
    fn check_primary_key(
        &self,
        referenced_tables: &[(Rc<BTreeTable>, String)],
    ) -> Result<Option<usize>> {
        match self {
            ast::Expr::Id(ident) => {
                let ident = normalize_ident(&ident.0);
                let tables = referenced_tables
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (t, _))| {
                        if t.get_column(&ident).map_or(false, |(_, c)| c.primary_key) {
                            Some(i)
                        } else {
                            None
                        }
                    });

                let mut matches = 0;
                let mut matching_tbl = None;

                for tbl in tables {
                    matching_tbl = Some(tbl);
                    matches += 1;
                    if matches > 1 {
                        crate::bail_parse_error!("ambiguous column name {}", ident)
                    }
                }

                Ok(matching_tbl)
            }
            ast::Expr::Qualified(tbl, ident) => {
                let tbl = normalize_ident(&tbl.0);
                let ident = normalize_ident(&ident.0);
                let table = referenced_tables.iter().enumerate().find(|(_, (t, t_id))| {
                    *t_id == tbl && t.get_column(&ident).map_or(false, |(_, c)| c.primary_key)
                });

                if table.is_none() {
                    return Ok(None);
                }

                let table = table.unwrap();

                Ok(Some(table.0))
            }
            _ => Ok(None),
        }
    }
    fn check_constant(&self) -> Result<Option<ConstantPredicate>> {
        match self {
            ast::Expr::Literal(lit) => match lit {
                ast::Literal::Null => Ok(Some(ConstantPredicate::AlwaysFalse)),
                ast::Literal::Numeric(b) => {
                    if let Ok(int_value) = b.parse::<i64>() {
                        return Ok(Some(if int_value == 0 {
                            ConstantPredicate::AlwaysFalse
                        } else {
                            ConstantPredicate::AlwaysTrue
                        }));
                    }
                    if let Ok(float_value) = b.parse::<f64>() {
                        return Ok(Some(if float_value == 0.0 {
                            ConstantPredicate::AlwaysFalse
                        } else {
                            ConstantPredicate::AlwaysTrue
                        }));
                    }

                    Ok(None)
                }
                ast::Literal::String(s) => {
                    let without_quotes = s.trim_matches('\'');
                    if let Ok(int_value) = without_quotes.parse::<i64>() {
                        return Ok(Some(if int_value == 0 {
                            ConstantPredicate::AlwaysFalse
                        } else {
                            ConstantPredicate::AlwaysTrue
                        }));
                    }

                    if let Ok(float_value) = without_quotes.parse::<f64>() {
                        return Ok(Some(if float_value == 0.0 {
                            ConstantPredicate::AlwaysFalse
                        } else {
                            ConstantPredicate::AlwaysTrue
                        }));
                    }

                    Ok(Some(ConstantPredicate::AlwaysFalse))
                }
                _ => Ok(None),
            },
            ast::Expr::Unary(op, expr) => {
                if *op == ast::UnaryOperator::Not {
                    let trivial = expr.check_constant()?;
                    return Ok(trivial.map(|t| match t {
                        ConstantPredicate::AlwaysTrue => ConstantPredicate::AlwaysFalse,
                        ConstantPredicate::AlwaysFalse => ConstantPredicate::AlwaysTrue,
                    }));
                }

                if *op == ast::UnaryOperator::Negative {
                    let trivial = expr.check_constant()?;
                    return Ok(trivial);
                }

                Ok(None)
            }
            ast::Expr::InList { lhs: _, not, rhs } => {
                if rhs.is_none() {
                    return Ok(Some(if *not {
                        ConstantPredicate::AlwaysTrue
                    } else {
                        ConstantPredicate::AlwaysFalse
                    }));
                }
                let rhs = rhs.as_ref().unwrap();
                if rhs.is_empty() {
                    return Ok(Some(if *not {
                        ConstantPredicate::AlwaysTrue
                    } else {
                        ConstantPredicate::AlwaysFalse
                    }));
                }

                Ok(None)
            }
            ast::Expr::Binary(lhs, op, rhs) => {
                let lhs_trivial = lhs.check_constant()?;
                let rhs_trivial = rhs.check_constant()?;
                match op {
                    ast::Operator::And => {
                        if lhs_trivial == Some(ConstantPredicate::AlwaysFalse)
                            || rhs_trivial == Some(ConstantPredicate::AlwaysFalse)
                        {
                            return Ok(Some(ConstantPredicate::AlwaysFalse));
                        }
                        if lhs_trivial == Some(ConstantPredicate::AlwaysTrue)
                            && rhs_trivial == Some(ConstantPredicate::AlwaysTrue)
                        {
                            return Ok(Some(ConstantPredicate::AlwaysTrue));
                        }

                        Ok(None)
                    }
                    ast::Operator::Or => {
                        if lhs_trivial == Some(ConstantPredicate::AlwaysTrue)
                            || rhs_trivial == Some(ConstantPredicate::AlwaysTrue)
                        {
                            return Ok(Some(ConstantPredicate::AlwaysTrue));
                        }
                        if lhs_trivial == Some(ConstantPredicate::AlwaysFalse)
                            && rhs_trivial == Some(ConstantPredicate::AlwaysFalse)
                        {
                            return Ok(Some(ConstantPredicate::AlwaysFalse));
                        }

                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}

pub fn try_extract_rowid_comparison_expression(
    expr: ast::Expr,
    table_index: usize,
    referenced_tables: &[(Rc<BTreeTable>, String)],
) -> Result<(bool, ast::Expr)> {
    match expr {
        ast::Expr::Binary(lhs, ast::Operator::Equals, rhs) => {
            if let Some(lhs_table_index) = lhs.check_primary_key(referenced_tables)? {
                if lhs_table_index == table_index {
                    return Ok((true, *rhs));
                }
            }

            if let Some(rhs_table_index) = rhs.check_primary_key(referenced_tables)? {
                if rhs_table_index == table_index {
                    return Ok((true, *lhs));
                }
            }

            Ok((false, ast::Expr::Binary(lhs, ast::Operator::Equals, rhs)))
        }
        _ => Ok((false, expr)),
    }
}

trait TakeOwnership {
    fn take_ownership(&mut self) -> Self;
}

impl TakeOwnership for ast::Expr {
    fn take_ownership(&mut self) -> Self {
        std::mem::replace(self, ast::Expr::Literal(ast::Literal::Null))
    }
}

impl TakeOwnership for Operator {
    fn take_ownership(&mut self) -> Self {
        std::mem::replace(self, Operator::Nothing)
    }
}