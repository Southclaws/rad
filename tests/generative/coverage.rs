use std::collections::BTreeSet;

use rad::engine::catalog::model::ScalarType;
use rad::engine::exec::ErrorReason;
use rad::engine::lir::{
    AggregateFunction, BinaryOp, Expr, JoinKind, RecursiveAccumulation, Relation, RootCardinality,
    SetQuantifier, TextComparison, TextMatchPart, UnaryOp, Value,
};

use super::{Case, invalid, recursive_case};

#[test]
fn seed_and_decision_tape_reproduce_the_complete_case() {
    let first = Case::from_seed(0x7261_642d_7365_6564);
    let replayed = Case::from_seed(0x7261_642d_7365_6564);
    let from_tape = Case::generate(first.decisions.clone());

    for candidate in [&replayed, &from_tape] {
        assert_eq!(candidate.decisions, first.decisions);
        assert_eq!(candidate.catalog, first.catalog);
        assert_eq!(candidate.data, first.data);
        assert_eq!(candidate.query, first.query);
        assert_eq!(candidate.ordered, first.ordered);
    }
    assert_ne!(
        Case::from_seed(0x7261_642d_7365_6565).decisions,
        first.decisions
    );
}

#[test]
fn generator_reaches_every_supported_campaign_family() {
    let mut features = BTreeSet::new();
    for seed in 0..512 {
        let case = Case::from_seed(seed);
        relation(&case.query.root, &mut features);
        if !case.query.bindings.is_empty() {
            features.insert("binding");
        }
        for binding in case.query.bindings.values() {
            relation(binding, &mut features);
        }
        if case.ordered {
            features.insert("ordered_root");
        } else {
            features.insert("bag_root");
        }
        if case.data.values().flatten().any(|row| {
            row.values().any(|value| {
                matches!(value, Value::Float64(value) if value.to_bits() == (-0.0_f64).to_bits())
            })
        }) {
            features.insert("negative_zero_data");
        }
    }
    let expected = BTreeSet::from([
        "aggregate",
        "arithmetic",
        "bag_root",
        "binding",
        "branch",
        "concatenate",
        "crossing",
        "distinct",
        "except",
        "filter",
        "group_by",
        "intersect",
        "join",
        "left_join",
        "negative_zero_data",
        "nested_distinct",
        "ordered_root",
        "project",
        "scan",
        "slice",
        "text_match",
    ]);
    let missing = expected.difference(&features).collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "generator missed {missing:?}; reached {features:?}"
    );
}

#[test]
fn relational_generator_reaches_each_declared_shape() {
    let mut features = BTreeSet::new();
    for seed in 0..4_096 {
        let case = Case::from_seed(seed);
        features.insert(match case.query.cardinality {
            RootCardinality::Many => "root.many",
            RootCardinality::First => "root.first",
            RootCardinality::ExactlyOne => "root.exactly_one",
            RootCardinality::Scalar => "root.scalar",
        });
        detailed_relation(&case.query.root, &mut features);
        for binding in case.query.bindings.values() {
            detailed_relation(binding, &mut features);
        }
    }

    let expected = BTreeSet::from([
        "aggregate.avg",
        "aggregate.count_values",
        "aggregate.count_rows",
        "aggregate.grouped",
        "aggregate.max",
        "aggregate.min",
        "aggregate.sum",
        "binary.add",
        "binary.and",
        "binary.eq",
        "binary.gt",
        "binary.gte",
        "binary.lt",
        "binary.lte",
        "binary.ne",
        "binary.or",
        "binary.div",
        "binary.mul",
        "binary.sub",
        "branch",
        "branch.multiple_arms",
        "cast",
        "crossing.array",
        "crossing.exists",
        "crossing.first",
        "crossing.scalar",
        "except.all",
        "except.distinct",
        "intersect.all",
        "intersect.distinct",
        "join.inner",
        "join.left",
        "relation.aggregate",
        "relation.concatenate",
        "relation.distinct",
        "relation.except",
        "relation.filter",
        "relation.intersect",
        "relation.join",
        "relation.order",
        "relation.project",
        "relation.ref",
        "relation.rows",
        "relation.scan",
        "relation.slice",
        "root.exactly_one",
        "root.first",
        "root.many",
        "root.scalar",
        "project.spread",
        "slice.unbounded",
        "text_match.any_many",
        "text_match.exact",
        "text_match.literal",
        "text_match.unicode_simple_fold",
        "unary.is_null",
        "unary.is_not_null",
        "unary.negate",
        "unary.not",
    ]);
    assert_eq!(
        features, expected,
        "relational generator shape contract drifted"
    );
}

#[test]
fn recursive_generator_reaches_each_declared_shape() {
    let mut features = BTreeSet::new();
    for seed in 0..512 {
        let case = recursive_case(seed);
        detailed_relation(&case.query.root, &mut features);
        for binding in case.query.bindings.values() {
            detailed_relation(binding, &mut features);
        }
    }

    let expected = BTreeSet::from([
        "binary.add",
        "binary.eq",
        "join.inner",
        "recursive.accumulate_all",
        "recursive.accumulate_new",
        "relation.join",
        "relation.order",
        "relation.project",
        "relation.recursive",
        "relation.recursive_ref",
        "relation.ref",
        "relation.rows",
        "relation.scan",
        "unary.not",
    ]);
    assert_eq!(
        features, expected,
        "recursive generator shape contract drifted"
    );
}

#[test]
fn catalog_generator_reaches_each_declared_shape() {
    let mut features = BTreeSet::new();
    for seed in 0..4_096 {
        let case = Case::from_seed(seed);
        if case.catalog.tables.len() > 1 {
            features.insert("catalog.multiple_tables");
        }
        for table in &case.catalog.tables {
            if !table.foreign_keys.is_empty() {
                features.insert("catalog.foreign_key");
            }
            if !table.indexes.is_empty() {
                features.insert("catalog.index");
            }
            for column in &table.columns {
                features.insert(match column.scalar_type {
                    ScalarType::Text => "column.text",
                    ScalarType::Int64 => "column.int64",
                    ScalarType::Float64 => "column.float64",
                    ScalarType::Bool => "column.bool",
                });
                features.insert(if column.nullable {
                    "column.nullable"
                } else {
                    "column.required"
                });
            }
        }
        for row in case.data.values().flatten() {
            for value in row.values() {
                if matches!(value, Value::Null(_)) {
                    features.insert("data.null");
                }
            }
        }
        if case.data.values().any(Vec::is_empty) {
            features.insert("data.empty_table");
        }
        if case.data.values().any(|rows| !rows.is_empty()) {
            features.insert("data.populated_table");
        }
    }

    let expected = BTreeSet::from([
        "catalog.foreign_key",
        "catalog.index",
        "catalog.multiple_tables",
        "column.bool",
        "column.float64",
        "column.int64",
        "column.nullable",
        "column.required",
        "column.text",
        "data.empty_table",
        "data.null",
        "data.populated_table",
    ]);
    assert_eq!(
        features, expected,
        "catalog generator shape contract drifted"
    );
}

#[test]
fn invalid_generator_reaches_each_declared_reason() {
    let case = Case::from_seed(0x696e_7661_6c69_642d);
    let reached = invalid::variants(&case)
        .into_iter()
        .map(|variant| (variant.name, variant.reason))
        .collect::<Vec<_>>();
    let expected = [
        ("unknown table", ErrorReason::UnknownTable),
        ("unknown column", ErrorReason::UnknownColumn),
        ("unknown scope", ErrorReason::UnknownScope),
        ("unknown binding", ErrorReason::UnknownBinding),
        ("duplicate scope", ErrorReason::DuplicateScope),
        ("non-boolean predicate", ErrorReason::TypeMismatch),
        ("projection collision", ErrorReason::ProjectionCollision),
        (
            "nondeterministic collection",
            ErrorReason::NondeterministicOrder,
        ),
        ("scalar arity", ErrorReason::ScalarArity),
        ("empty scope", ErrorReason::Invalid),
        ("dependent join", ErrorReason::DependentJoin),
        ("binding cycle", ErrorReason::BindingCycle),
        (
            "binding output collision",
            ErrorReason::BindingOutputCollision,
        ),
        ("crossing in branch", ErrorReason::Invalid),
        ("recursive step without recursive ref", ErrorReason::Invalid),
        ("recursive ref in non-monotone slice", ErrorReason::Invalid),
        (
            "recursive ref names another binding",
            ErrorReason::UnknownBinding,
        ),
        ("recursive step shape mismatch", ErrorReason::TypeMismatch),
    ];
    assert_eq!(reached, expected);
}

fn detailed_relation(value: &Relation, features: &mut BTreeSet<&'static str>) {
    match value {
        Relation::Scan { .. } => {
            features.insert("relation.scan");
        }
        Relation::Rows { .. } => {
            features.insert("relation.rows");
        }
        Relation::Filter { input, predicate } => {
            features.insert("relation.filter");
            detailed_expression(predicate, features);
            detailed_relation(input, features);
        }
        Relation::Project {
            input,
            spread,
            fields,
            ..
        } => {
            features.insert("relation.project");
            if !spread.is_empty() {
                features.insert("project.spread");
            }
            for field in fields {
                detailed_expression(&field.expression, features);
            }
            detailed_relation(input, features);
        }
        Relation::Join {
            left,
            right,
            kind,
            on,
        } => {
            features.insert("relation.join");
            features.insert(match kind {
                JoinKind::Inner => "join.inner",
                JoinKind::Left => "join.left",
            });
            detailed_expression(on, features);
            detailed_relation(left, features);
            detailed_relation(right, features);
        }
        Relation::Concatenate { inputs, .. } => {
            features.insert("relation.concatenate");
            for input in inputs {
                detailed_relation(input, features);
            }
        }
        Relation::Intersect {
            left,
            right,
            quantifier,
            ..
        } => {
            features.insert("relation.intersect");
            features.insert(match quantifier {
                SetQuantifier::All => "intersect.all",
                SetQuantifier::Distinct => "intersect.distinct",
            });
            detailed_relation(left, features);
            detailed_relation(right, features);
        }
        Relation::Except {
            left,
            right,
            quantifier,
            ..
        } => {
            features.insert("relation.except");
            features.insert(match quantifier {
                SetQuantifier::All => "except.all",
                SetQuantifier::Distinct => "except.distinct",
            });
            detailed_relation(left, features);
            detailed_relation(right, features);
        }
        Relation::Aggregate {
            input,
            groups,
            terms,
            ..
        } => {
            features.insert("relation.aggregate");
            if !groups.is_empty() {
                features.insert("aggregate.grouped");
            }
            for group in groups {
                detailed_expression(&group.expression, features);
            }
            for term in terms {
                features.insert(match (term.function, term.argument.is_some()) {
                    (AggregateFunction::Count, false) => "aggregate.count_rows",
                    (AggregateFunction::Count, true) => "aggregate.count_values",
                    (AggregateFunction::Sum, _) => "aggregate.sum",
                    (AggregateFunction::Average, _) => "aggregate.avg",
                    (AggregateFunction::Min, _) => "aggregate.min",
                    (AggregateFunction::Max, _) => "aggregate.max",
                });
                if let Some(argument) = &term.argument {
                    detailed_expression(argument, features);
                }
            }
            detailed_relation(input, features);
        }
        Relation::Order { input, terms } => {
            features.insert("relation.order");
            for term in terms {
                detailed_expression(&term.expression, features);
            }
            detailed_relation(input, features);
        }
        Relation::Slice { input, limit, .. } => {
            features.insert("relation.slice");
            if limit.is_none() {
                features.insert("slice.unbounded");
            }
            detailed_relation(input, features);
        }
        Relation::Ref { .. } => {
            features.insert("relation.ref");
        }
        Relation::RecursiveRef { .. } => {
            features.insert("relation.recursive_ref");
        }
        Relation::Recursive {
            anchor,
            step,
            accumulation,
        } => {
            features.insert("relation.recursive");
            features.insert(match accumulation {
                RecursiveAccumulation::All => "recursive.accumulate_all",
                RecursiveAccumulation::New => "recursive.accumulate_new",
            });
            detailed_relation(anchor, features);
            detailed_relation(step, features);
        }
        Relation::Distinct(input) => {
            features.insert("relation.distinct");
            detailed_relation(input, features);
        }
    }
}

fn detailed_expression(value: &Expr, features: &mut BTreeSet<&'static str>) {
    match value {
        Expr::Literal(_) | Expr::Column { .. } => {}
        Expr::Unary { op, expression } => {
            features.insert(match op {
                UnaryOp::Not => "unary.not",
                UnaryOp::Negate => "unary.negate",
                UnaryOp::IsNull => "unary.is_null",
                UnaryOp::IsNotNull => "unary.is_not_null",
            });
            detailed_expression(expression, features);
        }
        Expr::Binary { op, left, right } => {
            features.insert(match op {
                BinaryOp::Eq => "binary.eq",
                BinaryOp::Ne => "binary.ne",
                BinaryOp::Lt => "binary.lt",
                BinaryOp::Lte => "binary.lte",
                BinaryOp::Gt => "binary.gt",
                BinaryOp::Gte => "binary.gte",
                BinaryOp::And => "binary.and",
                BinaryOp::Or => "binary.or",
                BinaryOp::Add => "binary.add",
                BinaryOp::Sub => "binary.sub",
                BinaryOp::Mul => "binary.mul",
                BinaryOp::Div => "binary.div",
            });
            detailed_expression(left, features);
            detailed_expression(right, features);
        }
        Expr::Cast { expression, .. } => {
            features.insert("cast");
            detailed_expression(expression, features);
        }
        Expr::Branch { arms, otherwise } => {
            features.insert("branch");
            if arms.len() > 1 {
                features.insert("branch.multiple_arms");
            }
            for arm in arms {
                detailed_expression(&arm.when, features);
                detailed_expression(&arm.then, features);
            }
            detailed_expression(otherwise, features);
        }
        Expr::TextMatch {
            value,
            parts,
            comparison,
        } => {
            features.insert(match comparison {
                TextComparison::Exact => "text_match.exact",
                TextComparison::UnicodeSimpleFold => "text_match.unicode_simple_fold",
            });
            for part in parts {
                features.insert(match part {
                    TextMatchPart::Literal(_) => "text_match.literal",
                    TextMatchPart::AnyMany => "text_match.any_many",
                });
            }
            detailed_expression(value, features);
        }
        Expr::Exists(relation) => {
            features.insert("crossing.exists");
            detailed_relation(relation, features);
        }
        Expr::First(relation) => {
            features.insert("crossing.first");
            detailed_relation(relation, features);
        }
        Expr::Scalar(relation) => {
            features.insert("crossing.scalar");
            detailed_relation(relation, features);
        }
        Expr::Array(relation) => {
            features.insert("crossing.array");
            detailed_relation(relation, features);
        }
    }
}

fn relation(value: &Relation, features: &mut BTreeSet<&'static str>) {
    match value {
        Relation::Scan { .. } => {
            features.insert("scan");
        }
        Relation::Rows { .. } => {
            features.insert("rows");
        }
        Relation::Filter { input, predicate } => {
            features.insert("filter");
            expression(predicate, features);
            relation(input, features);
        }
        Relation::Project { input, fields, .. } => {
            features.insert("project");
            for field in fields {
                expression(&field.expression, features);
            }
            relation(input, features);
        }
        Relation::Join {
            left,
            right,
            kind,
            on,
        } => {
            features.insert("join");
            if *kind == rad::engine::lir::JoinKind::Left {
                features.insert("left_join");
            }
            expression(on, features);
            relation(left, features);
            relation(right, features);
        }
        Relation::Concatenate { inputs, .. } => {
            features.insert("concatenate");
            for input in inputs {
                relation(input, features);
            }
        }
        Relation::Intersect { left, right, .. } => {
            features.insert("intersect");
            relation(left, features);
            relation(right, features);
        }
        Relation::Except { left, right, .. } => {
            features.insert("except");
            relation(left, features);
            relation(right, features);
        }
        Relation::Aggregate {
            input,
            groups,
            terms,
            ..
        } => {
            features.insert("aggregate");
            if !groups.is_empty() {
                features.insert("group_by");
            }
            for group in groups {
                expression(&group.expression, features);
            }
            for term in terms {
                if let Some(argument) = &term.argument {
                    expression(argument, features);
                }
            }
            relation(input, features);
        }
        Relation::Order { input, terms } => {
            for term in terms {
                expression(&term.expression, features);
            }
            relation(input, features);
        }
        Relation::Slice { input, .. } => {
            features.insert("slice");
            relation(input, features);
        }
        Relation::Ref { .. } => {
            features.insert("binding");
        }
        Relation::RecursiveRef { .. } | Relation::Recursive { .. } => {
            features.insert("recursive");
        }
        Relation::Distinct(input) => {
            features.insert("distinct");
            if has_nested_projection(input) {
                features.insert("nested_distinct");
            }
            relation(input, features);
        }
    }
}

fn has_nested_projection(value: &Relation) -> bool {
    matches!(
        value,
        Relation::Project { fields, .. }
            if fields.iter().any(|field| {
                matches!(field.expression, Expr::First(_) | Expr::Array(_))
            })
    )
}

fn expression(value: &Expr, features: &mut BTreeSet<&'static str>) {
    match value {
        Expr::Literal(_) | Expr::Column { .. } => {}
        Expr::Unary {
            expression: child, ..
        }
        | Expr::Cast {
            expression: child, ..
        } => {
            expression(child, features);
        }
        Expr::Binary { op, left, right } => {
            if matches!(
                op,
                rad::engine::lir::BinaryOp::Add
                    | rad::engine::lir::BinaryOp::Sub
                    | rad::engine::lir::BinaryOp::Mul
                    | rad::engine::lir::BinaryOp::Div
            ) {
                features.insert("arithmetic");
            }
            expression(left, features);
            expression(right, features);
        }
        Expr::Branch { arms, otherwise } => {
            features.insert("branch");
            for arm in arms {
                expression(&arm.when, features);
                expression(&arm.then, features);
            }
            expression(otherwise, features);
        }
        Expr::TextMatch { value, .. } => {
            features.insert("text_match");
            expression(value, features);
        }
        Expr::Exists(relation_value)
        | Expr::First(relation_value)
        | Expr::Scalar(relation_value)
        | Expr::Array(relation_value) => {
            features.insert("crossing");
            relation(relation_value, features);
        }
    }
}
