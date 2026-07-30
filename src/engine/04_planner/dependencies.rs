//! Compute physical decode liveness and immutable catalog compatibility fences.

use crate::engine::catalog::model::{CatalogDependencies, Column, Table};
use crate::engine::lir::bound::{self, RelationNode, SlotSet};

use super::physical::{BindingPlanKind, Node, Plan};

pub(super) fn prepare_catalog_dependencies(plan: &mut Plan) {
    let required = required_slots(plan);
    let mut dependencies = CatalogDependencies::default();
    walk_plan_mut(plan, &mut |node| match node {
        Node::PrimaryKeyGet {
            scan,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let RelationNode::Scan { table, .. } = &scan.node else {
                unreachable!()
            };
            let mut columns = decode_columns.clone();
            append_named_columns(&mut columns, table, &table.primary_key);
            dependencies.add_table_read(table, &columns);
        }
        Node::TableScan {
            scan,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let RelationNode::Scan { table, .. } = &scan.node else {
                unreachable!()
            };
            dependencies.add_table_read(table, decode_columns);
        }
        Node::IndexRangeScan {
            scan,
            index,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let RelationNode::Scan { table, .. } = &scan.node else {
                unreachable!()
            };
            let mut columns = decode_columns.clone();
            let names = table
                .index_column_names(index)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            append_named_columns(&mut columns, table, &names);
            dependencies.add_index_read(table, index, &columns);
        }
        _ => {}
    });
    plan.dependencies = dependencies;
}

fn required_slots(plan: &Plan) -> SlotSet {
    let mut required = SlotSet::new(plan.output.slots());
    for binding in &plan.bindings {
        required = required.union(&SlotSet::new(binding.output.slots()));
        if let BindingPlanKind::Recursive {
            step_output: output,
            ..
        } = &binding.kind
        {
            required = required.union(&SlotSet::new(output.slots()));
        }
    }
    walk_plan(plan, &mut |node| match node {
        Node::Filter { predicate, .. } => add_expr(&mut required, predicate),
        Node::Attach { specifications, .. } => {
            for specification in specifications {
                required = required.union(&SlotSet::new(specification.output.slots()));
            }
        }
        Node::Project { fields, .. } => {
            for field in fields {
                add_expr(&mut required, &field.expression);
            }
        }
        Node::Sort { terms, .. } => {
            for term in terms {
                add_expr(&mut required, &term.expression);
            }
        }
        Node::NestedLoopJoin { on, .. } => add_expr(&mut required, on),
        Node::Concatenate { input_outputs, .. } => {
            for output in input_outputs {
                required = required.union(&SlotSet::new(output.slots()));
            }
        }
        Node::Intersect {
            left_output,
            right_output,
            ..
        }
        | Node::Except {
            left_output,
            right_output,
            ..
        } => {
            required = required.union(&SlotSet::new(left_output.slots()));
            required = required.union(&SlotSet::new(right_output.slots()));
        }
        Node::Distinct { output, .. } => {
            required = required.union(&SlotSet::new(output.slots()));
        }
        Node::Aggregate { groups, terms, .. } => {
            for group in groups {
                add_expr(&mut required, &group.expression);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    add_expr(&mut required, argument);
                }
            }
        }
        _ => {}
    });
    required
}

fn add_expr(required: &mut SlotSet, expression: &bound::Expr) {
    *required = required.union(&expression.free_slots());
}

fn decode_columns_for(scan: &bound::Relation, required: &SlotSet) -> Vec<Column> {
    let RelationNode::Scan { table, .. } = &scan.node else {
        unreachable!()
    };
    scan.output()
        .fields
        .iter()
        .zip(&table.columns)
        .filter(|(field, _)| required.contains(field.slot))
        .map(|(_, column)| column.clone())
        .collect()
}

fn append_named_columns(columns: &mut Vec<Column>, table: &Table, names: &[String]) {
    for name in names {
        let column = table
            .column(name)
            .expect("bound access must reference an existing column");
        if !columns.iter().any(|existing| existing.id == column.id) {
            columns.push(column.clone());
        }
    }
}

fn walk_plan(plan: &Plan, visitor: &mut impl FnMut(&Node)) {
    for binding in &plan.bindings {
        match &binding.kind {
            BindingPlanKind::Derived { plan, .. } => plan.walk(visitor),
            BindingPlanKind::Recursive { anchor, step, .. } => {
                anchor.walk(visitor);
                step.walk(visitor);
            }
        }
    }
    plan.root.walk(visitor);
}

fn walk_plan_mut(plan: &mut Plan, visitor: &mut impl FnMut(&mut Node)) {
    for binding in &mut plan.bindings {
        match &mut binding.kind {
            BindingPlanKind::Derived { plan, .. } => walk_node_mut(plan, visitor),
            BindingPlanKind::Recursive { anchor, step, .. } => {
                walk_node_mut(anchor, visitor);
                walk_node_mut(step, visitor);
            }
        }
    }
    walk_node_mut(&mut plan.root, visitor);
}

fn walk_node_mut(node: &mut Node, visitor: &mut impl FnMut(&mut Node)) {
    visitor(node);
    match node {
        Node::PrimaryKeyGet { .. }
        | Node::TableScan { .. }
        | Node::Rows(_)
        | Node::IndexRangeScan { .. }
        | Node::Reference { .. }
        | Node::RecursiveReference { .. } => {}
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Sort { input, .. }
        | Node::Slice { input, .. }
        | Node::Distinct { input, .. }
        | Node::Aggregate { input, .. } => walk_node_mut(input, visitor),
        Node::Attach {
            input,
            specifications,
        } => {
            walk_node_mut(input, visitor);
            for specification in specifications {
                walk_node_mut(&mut specification.plan, visitor);
            }
        }
        Node::NestedLoopJoin { left, right, .. }
        | Node::Intersect { left, right, .. }
        | Node::Except { left, right, .. } => {
            walk_node_mut(left, visitor);
            walk_node_mut(right, visitor);
        }
        Node::Concatenate { inputs, .. } => {
            for input in inputs {
                walk_node_mut(input, visitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::bound::{self, BoundAggregateTerm, ProjectField};
    use crate::engine::lir::{AggregateFunction, Kind, SlotId, Type};
    use crate::engine::planner::physical::Node;
    use crate::engine::planner::test_support::{column, query, scan};
    use crate::engine::planner::{PlanOptions, plan_query};

    #[test]
    fn projection_decodes_and_fences_only_observed_columns() {
        let scan = scan();
        let projected = bound::Relation::project(
            scan.clone(),
            "p",
            vec![ProjectField {
                name: "status".into(),
                slot: SlotId(3),
                expression: column(&scan, "status"),
            }],
        );
        let plan = plan_query(&query(projected, 4), PlanOptions::default());

        assert_eq!(plan.dependencies.table_existence.len(), 1);
        assert_eq!(plan.dependencies.index_access.len(), 0);
        assert_eq!(
            plan.dependencies
                .column_values
                .iter()
                .map(|dependency| dependency.column_name.as_str())
                .collect::<Vec<_>>(),
            ["status"]
        );
        let Node::Project { input, .. } = &plan.root else {
            panic!("expected project")
        };
        let Node::TableScan { decode_columns, .. } = &**input else {
            panic!("expected table scan")
        };
        assert_eq!(
            decode_columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["status"]
        );
    }

    #[test]
    fn count_rows_requires_table_existence_but_no_cell_values() {
        let aggregate = bound::Relation::aggregate(
            scan(),
            Vec::new(),
            vec![BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "count".into(),
                slot: SlotId(3),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        );
        let plan = plan_query(&query(aggregate, 4), PlanOptions::default());

        assert_eq!(plan.dependencies.table_existence.len(), 1);
        assert!(plan.dependencies.column_values.is_empty());
        let Node::Aggregate { input, .. } = &plan.root else {
            panic!("expected aggregate")
        };
        let Node::TableScan { decode_columns, .. } = &**input else {
            panic!("expected table scan")
        };
        assert!(decode_columns.is_empty());
    }
}
