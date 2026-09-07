//! Compute physical decode liveness and immutable catalog compatibility fences.

use crate::engine::catalog::model::{CatalogDependencies, Column, Table};
use crate::engine::lir::bound::{self, SlotSet};

use super::physical::{BindingPlanKind, NodeKind, Plan};

pub(super) fn prepare_catalog_dependencies(plan: &mut Plan) {
    let required = required_slots(plan);
    let mut dependencies = CatalogDependencies::default();
    plan.walk_mut(&mut |node| match &mut node.kind {
        NodeKind::PrimaryKeyGet {
            scan,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let table = scan.scan_table();
            let mut columns = decode_columns.clone();
            append_named_columns(&mut columns, table, &table.primary_key);
            dependencies.add_table_read(table, &columns);
        }
        NodeKind::TableScan {
            scan,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let table = scan.scan_table();
            dependencies.add_table_read(table, decode_columns);
        }
        NodeKind::IndexRangeScan {
            scan,
            index,
            decode_columns,
            ..
        } => {
            *decode_columns = decode_columns_for(scan, &required);
            let table = scan.scan_table();
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
    plan.walk(&mut |node| match &node.kind {
        NodeKind::Filter { predicate, .. } => add_expr(&mut required, predicate),
        NodeKind::Attach { specifications, .. } => {
            for specification in specifications {
                required = required.union(&SlotSet::new(specification.output.slots()));
            }
        }
        NodeKind::Project { fields, .. } => {
            for field in fields {
                add_expr(&mut required, &field.expression);
            }
        }
        NodeKind::Sort { terms, .. } => {
            for term in terms {
                add_expr(&mut required, &term.expression);
            }
        }
        NodeKind::NestedLoopJoin { on, .. }
        | NodeKind::HashJoin { on, .. }
        | NodeKind::IndexedLookupJoin { on, .. } => add_expr(&mut required, on),
        NodeKind::ShreddedYannakakisJoin { edges, .. } => {
            for edge in edges {
                for key in &edge.keys {
                    required = required.union(&SlotSet::new([key.left.slot, key.right.slot]));
                }
            }
        }
        NodeKind::PredicateTransferJoin { schedule, .. } => {
            for pass in [&schedule.forward, &schedule.backward] {
                for edge in &pass.edges {
                    for key in &edge.keys {
                        required = required.union(&SlotSet::new([key.left.slot, key.right.slot]));
                    }
                }
            }
        }
        NodeKind::Concatenate { input_outputs, .. } => {
            for output in input_outputs {
                required = required.union(&SlotSet::new(output.slots()));
            }
        }
        NodeKind::Intersect {
            left_output,
            right_output,
            ..
        }
        | NodeKind::Except {
            left_output,
            right_output,
            ..
        } => {
            required = required.union(&SlotSet::new(left_output.slots()));
            required = required.union(&SlotSet::new(right_output.slots()));
        }
        NodeKind::Distinct { output, .. } => {
            required = required.union(&SlotSet::new(output.slots()));
        }
        NodeKind::Aggregate { groups, terms, .. } => {
            for group in groups {
                add_expr(&mut required, &group.expression);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    add_expr(&mut required, argument);
                }
            }
        }
        NodeKind::GroupedHashJoinAggregate {
            keys,
            groups,
            terms,
            ..
        } => {
            for key in keys {
                required = required.union(&SlotSet::new([key.left.slot, key.right.slot]));
            }
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
    let table = scan.scan_table();
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

#[cfg(test)]
mod tests {
    use crate::engine::lir::bound::{self, BoundAggregateTerm, ProjectField};
    use crate::engine::lir::{AggregateFunction, Kind, SlotId, Type};
    use crate::engine::planner::physical::NodeKind;
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
        let NodeKind::Project { input, .. } = &plan.root.kind else {
            panic!("expected project")
        };
        let NodeKind::TableScan { decode_columns, .. } = &input.kind else {
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
        let NodeKind::Aggregate { input, .. } = &plan.root.kind else {
            panic!("expected aggregate")
        };
        let NodeKind::TableScan { decode_columns, .. } = &input.kind else {
            panic!("expected table scan")
        };
        assert!(decode_columns.is_empty());
    }

    #[test]
    fn count_non_null_slot_does_not_decode_the_slot() {
        let input = scan();
        let aggregate = bound::Relation::aggregate(
            input.clone(),
            Vec::new(),
            vec![BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: Some(column(&input, "id")),
                name: "count".into(),
                slot: SlotId(3),
                value_type: Type::scalar(Kind::Int64, false),
            }],
        );
        let plan = plan_query(&query(aggregate, 4), PlanOptions::default());

        let NodeKind::Aggregate { input, terms, .. } = &plan.root.kind else {
            panic!("expected aggregate")
        };
        assert!(terms[0].argument.is_none());
        let NodeKind::TableScan { decode_columns, .. } = &input.kind else {
            panic!("expected table scan")
        };
        assert!(decode_columns.is_empty());
    }
}
