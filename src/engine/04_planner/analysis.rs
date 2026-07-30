//! Pure constraint extraction and correlation classification over bound LIR.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::engine::lir::bound::{self, RelationNode};
use crate::engine::lir::{self, SlotId, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum ConstValue {
    Literal(Value),
    Outer(SlotId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeBound {
    pub value: Value,
    pub inclusive: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Domain {
    pub equality: Option<ConstValue>,
    pub lower: Option<RangeBound>,
    pub upper: Option<RangeBound>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanConstraints {
    pub scan: bound::Relation,
    pub columns: HashMap<String, Domain>,
}

pub fn underlying_scan(relation: &bound::Relation) -> Option<&bound::Relation> {
    match &relation.node {
        RelationNode::Scan { .. } => Some(relation),
        RelationNode::Filter { input, .. }
        | RelationNode::Order { input, .. }
        | RelationNode::Slice { input, .. } => underlying_scan(input),
        _ => None,
    }
}

pub fn extract_constraints(relation: &bound::Relation) -> Option<ScanConstraints> {
    let scan = underlying_scan(relation)?;
    let slot_to_column: HashMap<_, _> = scan
        .output()
        .fields
        .iter()
        .map(|field| (field.slot, field.name.clone()))
        .collect();
    let mut columns = HashMap::new();
    let mut poisoned = HashMap::new();

    let mut chain = relation;
    loop {
        match &chain.node {
            RelationNode::Filter { input, predicate } => {
                for conjunct in conjuncts(predicate) {
                    extract_conjunct(conjunct, scan, &slot_to_column, &mut columns, &mut poisoned);
                }
                chain = input;
            }
            RelationNode::Order { input, .. } | RelationNode::Slice { input, .. } => {
                chain = input;
            }
            _ => break,
        }
    }
    Some(ScanConstraints {
        scan: scan.clone(),
        columns,
    })
}

fn extract_conjunct(
    expression: &bound::Expr,
    scan: &bound::Relation,
    slot_to_column: &HashMap<SlotId, String>,
    columns: &mut HashMap<String, Domain>,
    poisoned: &mut HashMap<String, bool>,
) {
    let bound::Expr::Binary {
        op, left, right, ..
    } = expression
    else {
        return;
    };
    match op {
        lir::BinaryOp::Eq => {
            for (candidate, value) in [(&**left, &**right), (&**right, &**left)] {
                let bound::Expr::SlotRef { slot, .. } = candidate else {
                    continue;
                };
                let Some(column) = slot_to_column.get(slot) else {
                    continue;
                };
                if poisoned.get(column).copied().unwrap_or(false) {
                    continue;
                }
                let Some(value) = const_value(value, scan) else {
                    continue;
                };
                let domain = columns.entry(column.clone()).or_default();
                if domain
                    .equality
                    .as_ref()
                    .is_some_and(|existing| !same_const(existing, &value))
                {
                    poisoned.insert(column.clone(), true);
                    columns.remove(column);
                    continue;
                }
                domain.equality = Some(value);
                domain.lower = None;
                domain.upper = None;
            }
        }
        operation @ (lir::BinaryOp::Lt
        | lir::BinaryOp::Lte
        | lir::BinaryOp::Gt
        | lir::BinaryOp::Gte) => {
            if let (bound::Expr::SlotRef { slot, .. }, bound::Expr::Literal(value)) =
                (&**left, &**right)
                && !value.is_null()
            {
                add_range(
                    columns,
                    poisoned,
                    slot_to_column,
                    *slot,
                    *operation,
                    value.clone(),
                );
            } else if let (bound::Expr::Literal(value), bound::Expr::SlotRef { slot, .. }) =
                (&**left, &**right)
                && !value.is_null()
            {
                add_range(
                    columns,
                    poisoned,
                    slot_to_column,
                    *slot,
                    flip(*operation),
                    value.clone(),
                );
            }
        }
        _ => {}
    }
}

fn const_value(expression: &bound::Expr, scan: &bound::Relation) -> Option<ConstValue> {
    match expression {
        bound::Expr::Literal(value) if !value.is_null() => Some(ConstValue::Literal(value.clone())),
        bound::Expr::SlotRef { slot, .. } if !scan.produced().contains(*slot) => {
            Some(ConstValue::Outer(*slot))
        }
        _ => None,
    }
}

fn same_const(left: &ConstValue, right: &ConstValue) -> bool {
    match (left, right) {
        (ConstValue::Literal(left), ConstValue::Literal(right)) => left.storage_eq(right),
        (ConstValue::Outer(left), ConstValue::Outer(right)) => left == right,
        _ => false,
    }
}

fn add_range(
    columns: &mut HashMap<String, Domain>,
    poisoned: &HashMap<String, bool>,
    slot_to_column: &HashMap<SlotId, String>,
    slot: SlotId,
    operation: lir::BinaryOp,
    value: Value,
) {
    let Some(column) = slot_to_column.get(&slot) else {
        return;
    };
    if poisoned.get(column).copied().unwrap_or(false) {
        return;
    }
    let domain = columns.entry(column.clone()).or_default();
    if domain.equality.is_some() {
        return;
    }
    let bound = RangeBound {
        value,
        inclusive: matches!(operation, lir::BinaryOp::Lte | lir::BinaryOp::Gte),
    };
    match operation {
        lir::BinaryOp::Gt | lir::BinaryOp::Gte
            if domain
                .lower
                .as_ref()
                .is_none_or(|existing| tighter_lower(&bound, existing)) =>
        {
            domain.lower = Some(bound);
        }
        lir::BinaryOp::Lt | lir::BinaryOp::Lte
            if domain
                .upper
                .as_ref()
                .is_none_or(|existing| tighter_upper(&bound, existing)) =>
        {
            domain.upper = Some(bound);
        }
        _ => {}
    }
}

fn flip(operation: lir::BinaryOp) -> lir::BinaryOp {
    match operation {
        lir::BinaryOp::Lt => lir::BinaryOp::Gt,
        lir::BinaryOp::Lte => lir::BinaryOp::Gte,
        lir::BinaryOp::Gt => lir::BinaryOp::Lt,
        lir::BinaryOp::Gte => lir::BinaryOp::Lte,
        _ => operation,
    }
}

fn tighter_lower(candidate: &RangeBound, existing: &RangeBound) -> bool {
    candidate
        .value
        .compare(&existing.value)
        .is_ok_and(|ordering| {
            ordering == Ordering::Greater
                || (ordering == Ordering::Equal && !candidate.inclusive && existing.inclusive)
        })
}

fn tighter_upper(candidate: &RangeBound, existing: &RangeBound) -> bool {
    candidate
        .value
        .compare(&existing.value)
        .is_ok_and(|ordering| {
            ordering == Ordering::Less
                || (ordering == Ordering::Equal && !candidate.inclusive && existing.inclusive)
        })
}

fn conjuncts(expression: &bound::Expr) -> Vec<&bound::Expr> {
    if let bound::Expr::Binary {
        op: lir::BinaryOp::And,
        left,
        right,
        ..
    } = expression
    {
        let mut output = conjuncts(left);
        output.extend(conjuncts(right));
        output
    } else {
        vec![expression]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorrelationKind {
    Uncorrelated,
    Key,
    General,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrelationKey {
    pub inner_slot: SlotId,
    pub inner_column: String,
    pub outer_slot: SlotId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Correlation {
    pub kind: CorrelationKind,
    pub keys: Vec<CorrelationKey>,
}

pub fn classify_correlation(relation: &bound::Relation) -> Correlation {
    let free = relation.free_slots();
    if free.is_empty() {
        return Correlation {
            kind: CorrelationKind::Uncorrelated,
            keys: Vec::new(),
        };
    }
    let Some(scan) = scan_below(relation) else {
        return general_correlation();
    };
    let slot_to_column: HashMap<_, _> = scan
        .output()
        .fields
        .iter()
        .map(|field| (field.slot, field.name.clone()))
        .collect();
    let mut candidates = Vec::new();
    let mut chain = relation;
    loop {
        match &chain.node {
            RelationNode::Filter { input, predicate } => {
                for conjunct in conjuncts(predicate) {
                    let bound::Expr::Binary {
                        op: lir::BinaryOp::Eq,
                        left,
                        right,
                        ..
                    } = conjunct
                    else {
                        continue;
                    };
                    for (inner, outer) in [(&**left, &**right), (&**right, &**left)] {
                        let (
                            bound::Expr::SlotRef {
                                slot: inner_slot, ..
                            },
                            bound::Expr::SlotRef {
                                slot: outer_slot, ..
                            },
                        ) = (inner, outer)
                        else {
                            continue;
                        };
                        if let Some(column) = slot_to_column.get(inner_slot)
                            && !relation.produced().contains(*outer_slot)
                        {
                            candidates.push(CorrelationKey {
                                inner_slot: *inner_slot,
                                inner_column: column.clone(),
                                outer_slot: *outer_slot,
                            });
                        }
                    }
                }
                chain = input;
            }
            RelationNode::Order { input, .. }
            | RelationNode::Slice { input, .. }
            | RelationNode::Project { input, .. }
            | RelationNode::Aggregate { input, .. }
            | RelationNode::Distinct(input) => chain = input,
            _ => break,
        }
    }
    if candidates.is_empty() {
        return general_correlation();
    }
    let mut usage: HashMap<SlotId, usize> = HashMap::new();
    lir::inspect::walk_relation(relation, &mut |_| {}, &mut |expression| {
        if let bound::Expr::SlotRef { slot, .. } = expression
            && free.contains(*slot)
        {
            *usage.entry(*slot).or_default() += 1;
        }
    });
    for key in &candidates {
        if let Some(count) = usage.get_mut(&key.outer_slot) {
            *count = count.saturating_sub(1);
        }
    }
    if usage.values().any(|count| *count > 0) {
        return general_correlation();
    }
    Correlation {
        kind: CorrelationKind::Key,
        keys: candidates,
    }
}

fn general_correlation() -> Correlation {
    Correlation {
        kind: CorrelationKind::General,
        keys: Vec::new(),
    }
}

fn scan_below(relation: &bound::Relation) -> Option<&bound::Relation> {
    match &relation.node {
        RelationNode::Scan { .. } => Some(relation),
        RelationNode::Filter { input, .. }
        | RelationNode::Order { input, .. }
        | RelationNode::Slice { input, .. }
        | RelationNode::Project { input, .. }
        | RelationNode::Aggregate { input, .. }
        | RelationNode::Distinct(input) => scan_below(input),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{BinaryOp, Kind, SlotId, Type, Value};
    use crate::engine::planner::test_support::{column, scan, table};

    use super::*;

    fn literal(value: &str) -> bound::Expr {
        bound::Expr::literal(Value::Text(value.into()))
    }

    fn binary(op: BinaryOp, left: bound::Expr, right: bound::Expr) -> bound::Expr {
        bound::Expr::binary(op, left, right)
    }

    #[test]
    fn extraction_intersects_ranges_and_equality_wins() {
        let scan = scan();
        let predicate = binary(
            BinaryOp::And,
            binary(
                BinaryOp::And,
                binary(BinaryOp::Gt, column(&scan, "status"), literal("a")),
                binary(BinaryOp::Gte, column(&scan, "status"), literal("m")),
            ),
            binary(BinaryOp::Lt, column(&scan, "status"), literal("z")),
        );
        let filtered = bound::Relation::filter(scan.clone(), predicate);
        let domain = &extract_constraints(&filtered).unwrap().columns["status"];
        assert_eq!(
            domain.lower,
            Some(RangeBound {
                value: Value::Text("m".into()),
                inclusive: true,
            })
        );
        assert_eq!(
            domain.upper,
            Some(RangeBound {
                value: Value::Text("z".into()),
                inclusive: false,
            })
        );

        let equality = bound::Relation::filter(
            filtered,
            binary(BinaryOp::Eq, literal("open"), column(&scan, "status")),
        );
        let domain = &extract_constraints(&equality).unwrap().columns["status"];
        assert_eq!(
            domain.equality,
            Some(ConstValue::Literal(Value::Text("open".into())))
        );
        assert_eq!(domain.lower, None);
        assert_eq!(domain.upper, None);
    }

    #[test]
    fn conflicting_equalities_poison_a_column_and_unsafe_predicates_contribute_nothing() {
        let scan = scan();
        let conflicting = bound::Relation::filter(
            scan.clone(),
            binary(
                BinaryOp::And,
                binary(BinaryOp::Eq, column(&scan, "status"), literal("open")),
                binary(BinaryOp::Eq, column(&scan, "status"), literal("closed")),
            ),
        );
        assert!(
            !extract_constraints(&conflicting)
                .unwrap()
                .columns
                .contains_key("status")
        );

        let unsafe_predicate = bound::Relation::filter(
            scan.clone(),
            binary(
                BinaryOp::Or,
                binary(BinaryOp::Eq, column(&scan, "status"), literal("open")),
                binary(BinaryOp::Eq, column(&scan, "status"), literal("closed")),
            ),
        );
        assert!(
            extract_constraints(&unsafe_predicate)
                .unwrap()
                .columns
                .is_empty()
        );

        let null_equality = bound::Relation::filter(
            scan.clone(),
            binary(
                BinaryOp::Eq,
                column(&scan, "status"),
                bound::Expr::literal(Value::Null(crate::engine::catalog::model::ScalarType::Text)),
            ),
        );
        assert!(
            extract_constraints(&null_equality)
                .unwrap()
                .columns
                .is_empty()
        );
    }

    #[test]
    fn correlation_requires_outer_slots_to_be_used_only_as_scan_key_equalities() {
        let inner = bound::Relation::scan(table(), "inner", vec![SlotId(3), SlotId(4), SlotId(5)]);
        let outer = bound::Expr::slot(SlotId(0), "outer.id", Type::scalar(Kind::Text, false));
        let key_filter = bound::Relation::filter(
            inner.clone(),
            binary(BinaryOp::Eq, column(&inner, "board_id"), outer.clone()),
        );
        assert_eq!(
            classify_correlation(&key_filter),
            Correlation {
                kind: CorrelationKind::Key,
                keys: vec![CorrelationKey {
                    inner_slot: SlotId(4),
                    inner_column: "board_id".into(),
                    outer_slot: SlotId(0),
                }],
            }
        );

        let extra_outer_use = bound::Relation::filter(
            key_filter,
            binary(BinaryOp::Gt, column(&inner, "status"), outer),
        );
        assert_eq!(
            classify_correlation(&extra_outer_use).kind,
            CorrelationKind::General
        );
        assert_eq!(
            classify_correlation(&inner).kind,
            CorrelationKind::Uncorrelated
        );
    }
}
