//! Executor-facing physical operator tree.

use serde::Serialize;

use crate::engine::catalog::model::{CatalogDependencies, Column, Index};
use crate::engine::lir::bound::{self, BoundAggregateTerm, BoundGroupTerm, BoundOrderTerm};
use crate::engine::lir::{
    self, RecursiveAccumulation, RootCardinality, RowType, SetQuantifier, SlotId,
};

use super::analysis::{ConstValue, Correlation, EquiJoinKey, RangeBound};

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub bindings: Vec<BindingPlan>,
    pub root: Node,
    pub cardinality: RootCardinality,
    pub output: RowType,
    pub dependencies: CatalogDependencies,
    pub next_slot: SlotId,
    pub memo: super::memo::MemoReport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingStrategy {
    Materialize,
    Replay,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BindingPlan {
    pub name: String,
    pub output: RowType,
    pub sensitive: bool,
    pub kind: BindingPlanKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BindingPlanKind {
    Derived {
        plan: Box<Node>,
        strategy: BindingStrategy,
    },
    Recursive {
        anchor: Box<Node>,
        step: Box<Node>,
        step_output: RowType,
        accumulation: RecursiveAccumulation,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessDecision {
    pub candidates: Vec<AccessCandidate>,
    #[serde(default)]
    pub structural_fallback: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessCandidate {
    pub method: String,
    pub score: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_row_work: Option<AccessRowWork>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<AccessCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_basis: Option<AccessDecisionBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<AccessRejectionReason>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chosen: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessDecisionBasis {
    BoundedRowWork,
    CostDominance,
    PrimaryKey,
    Structural,
}

impl AccessDecisionBasis {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::BoundedRowWork => "bounded_row_work",
            Self::CostDominance => "cost_dominance",
            Self::PrimaryKey => "primary_key",
            Self::Structural => "structural",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRowWork {
    pub lower_bound: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_bound: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessRejectionReason {
    InconclusiveEvidence,
    MoreExpensive,
    OrderingRegression,
    OverlappingCost,
    StructuralFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessCost {
    pub expected_entries: super::estimator::Estimate,
    pub point_gets: AccessQuantity,
    pub range_scans: AccessQuantity,
    pub logical_row_operations: AccessQuantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded_bytes: Option<AccessQuantity>,
    pub ordering: AccessOrdering,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessQuantity {
    pub central: u64,
    pub lower_bound: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_bound: Option<u64>,
}

impl AccessQuantity {
    pub const fn exact(value: u64) -> Self {
        Self {
            central: value,
            lower_bound: value,
            upper_bound: Some(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessOrdering {
    NotRequired,
    Satisfied,
    SortRequired,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinDecision {
    pub candidates: Vec<JoinCandidate>,
    #[serde(default)]
    pub structural_fallback: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinCandidate {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<JoinCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_basis: Option<JoinDecisionBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<JoinRejectionReason>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chosen: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinDecisionBasis {
    CostDominance,
    Structural,
}

impl JoinDecisionBasis {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::CostDominance => "cost_dominance",
            Self::Structural => "structural",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinRejectionReason {
    MemoryLimit,
    MissingEvidence,
    MoreExpensive,
    OverlappingCost,
    StructuralFallback,
    UnsupportedInput,
    UnsupportedPredicate,
}

impl JoinRejectionReason {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::MemoryLimit => "memory_limit",
            Self::MissingEvidence => "missing_evidence",
            Self::MoreExpensive => "more_expensive",
            Self::OverlappingCost => "overlapping_cost",
            Self::StructuralFallback => "structural_fallback",
            Self::UnsupportedInput => "unsupported_input",
            Self::UnsupportedPredicate => "unsupported_predicate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinCost {
    pub expected_output_rows: super::estimator::Estimate,
    pub build_rows: AccessQuantity,
    pub probe_rows: AccessQuantity,
    pub lookup_requests: AccessQuantity,
    pub key_comparisons: AccessQuantity,
    pub residual_predicate_evaluations: AccessQuantity,
    pub logical_row_operations: AccessQuantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_retained_bytes: Option<AccessQuantity>,
    pub ordering: AccessOrdering,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinGraphDecision {
    pub classification: JoinGraphClassification,
    pub input_count: usize,
    pub edge_count: usize,
    pub root_input: usize,
    pub semijoin_passes: u32,
    pub predicate_transfer_passes: u32,
    pub candidates: Vec<JoinGraphCandidate>,
    #[serde(default)]
    pub structural_fallback: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinGraphClassification {
    Acyclic,
    Cyclic,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinGraphCandidate {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<JoinGraphCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_basis: Option<JoinGraphDecisionBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<JoinGraphRejectionReason>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chosen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinGraphCost {
    pub logical_row_operations: AccessQuantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduction_row_operations: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup_row_operations: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded_rows: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_row_operations: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_rows: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_bytes: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_paths: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_builds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_filter_paths: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned_filter_paths: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_input_scans: Option<AccessQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_schedule_root: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_retained_bytes: Option<AccessQuantity>,
    pub ordering: AccessOrdering,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinGraphDecisionBasis {
    BoundedNoRegret,
    Structural,
}

impl JoinGraphDecisionBasis {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::BoundedNoRegret => "bounded_no_regret",
            Self::Structural => "structural",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinGraphRejectionReason {
    CostOverlap,
    CyclicGraph,
    MemoryLimit,
    MissingEvidence,
    MoreExpensive,
    OrderingConflict,
    StructuralFallback,
}

impl JoinGraphRejectionReason {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::CostOverlap => "cost_overlap",
            Self::CyclicGraph => "cyclic_graph",
            Self::MemoryLimit => "memory_limit",
            Self::MissingEvidence => "missing_evidence",
            Self::MoreExpensive => "more_expensive",
            Self::OrderingConflict => "ordering_conflict",
            Self::StructuralFallback => "structural_fallback",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrossingKind {
    Exists,
    First,
    Scalar,
    Array,
}

impl CrossingKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Exists => "exists",
            Self::First => "first",
            Self::Scalar => "scalar",
            Self::Array => "array",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttachSpec {
    pub slot: SlotId,
    pub kind: CrossingKind,
    pub correlation: Correlation,
    pub plan: Node,
    pub output: RowType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalField {
    pub name: String,
    pub slot: SlotId,
    pub expression: bound::Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeSpec {
    pub column: String,
    pub lower: Option<RangeBound>,
    pub upper: Option<RangeBound>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShreddedJoinEdge {
    pub parent: usize,
    pub child: usize,
    pub keys: Vec<EquiJoinKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PredicateTransferEdge {
    pub source: usize,
    pub target: usize,
    pub keys: Vec<EquiJoinKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PredicateTransferPass {
    pub order: Vec<usize>,
    pub edges: Vec<PredicateTransferEdge>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PredicateTransferSchedule {
    pub root_input: usize,
    pub forward: PredicateTransferPass,
    pub backward: PredicateTransferPass,
    pub pruned_paths: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredicateTransferRuntimePolicy {
    pub block_rows: u32,
    pub build_sample_rows: u64,
    pub build_selectivity_threshold_bps: u16,
    pub build_progress_threshold_bps: u16,
    pub probe_sample_rows: u64,
    pub probe_stop_threshold_bps: u16,
}

/// A physical operator plus the logical relation it implements, when the
/// planner can state that unambiguously. Attribution is inert: it never
/// affects planning, execution shape, or which path runs, so a plan is
/// identical whether or not anything is measuring it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub attribution: Option<lir::fingerprint::Fingerprint>,
    pub kind: NodeKind,
}

impl From<NodeKind> for Node {
    fn from(kind: NodeKind) -> Self {
        Self {
            attribution: None,
            kind,
        }
    }
}

impl NodeKind {
    /// This operator with no logical counterpart recorded.
    pub fn bare(self) -> Node {
        Node::from(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    PrimaryKeyGet {
        scan: Box<bound::Relation>,
        key: Vec<ConstValue>,
        decode_columns: Vec<Column>,
        access: AccessDecision,
    },
    TableScan {
        scan: Box<bound::Relation>,
        decode_columns: Vec<Column>,
        access: AccessDecision,
    },
    Rows(bound::Relation),
    IndexRangeScan {
        scan: Box<bound::Relation>,
        index: Index,
        equality_prefix: Vec<ConstValue>,
        range: Option<RangeSpec>,
        decode_columns: Vec<Column>,
        access: AccessDecision,
    },
    Filter {
        input: Box<Node>,
        predicate: bound::Expr,
    },
    Attach {
        input: Box<Node>,
        specifications: Vec<AttachSpec>,
    },
    Project {
        input: Box<Node>,
        fields: Vec<PhysicalField>,
    },
    Sort {
        input: Box<Node>,
        terms: Vec<BoundOrderTerm>,
    },
    Slice {
        input: Box<Node>,
        offset: usize,
        limit: Option<usize>,
    },
    Reference {
        binding: String,
        output: RowType,
        canonical: Vec<SlotId>,
    },
    RecursiveReference {
        binding: String,
        output: RowType,
        canonical: Vec<SlotId>,
    },
    Distinct {
        input: Box<Node>,
        output: RowType,
    },
    NestedLoopJoin {
        left: Box<Node>,
        right: Box<Node>,
        kind: lir::JoinKind,
        on: bound::Expr,
        keys: Vec<EquiJoinKey>,
        right_output: RowType,
        decision: JoinDecision,
    },
    HashJoin {
        left: Box<Node>,
        right: Box<Node>,
        kind: lir::JoinKind,
        on: bound::Expr,
        keys: Vec<EquiJoinKey>,
        right_output: RowType,
        memory_limit_bytes: u64,
        decision: JoinDecision,
    },
    IndexedLookupJoin {
        left: Box<Node>,
        right: Box<Node>,
        kind: lir::JoinKind,
        on: bound::Expr,
        keys: Vec<EquiJoinKey>,
        right_output: RowType,
        decision: JoinDecision,
    },
    JoinGraphChoice {
        input: Box<Node>,
        decision: JoinGraphDecision,
    },
    ShreddedYannakakisJoin {
        inputs: Vec<Node>,
        edges: Vec<ShreddedJoinEdge>,
        root_input: usize,
        output: RowType,
        memory_limit_bytes: u64,
        logical_row_operations: AccessQuantity,
        estimated_peak_retained_bytes: AccessQuantity,
    },
    PredicateTransferJoin {
        inputs: Vec<Node>,
        schedule: PredicateTransferSchedule,
        join_plan: Box<Node>,
        output: RowType,
        bits_per_key: u8,
        hash_functions: u8,
        runtime_policy: PredicateTransferRuntimePolicy,
        memory_limit_bytes: u64,
        logical_row_operations: AccessQuantity,
        estimated_peak_retained_bytes: AccessQuantity,
    },
    PredicateTransferInput {
        input: usize,
        output: RowType,
    },
    Concatenate {
        inputs: Vec<Node>,
        input_outputs: Vec<RowType>,
        output: RowType,
    },
    Intersect {
        left: Box<Node>,
        right: Box<Node>,
        quantifier: SetQuantifier,
        left_output: RowType,
        right_output: RowType,
        output: RowType,
    },
    Except {
        left: Box<Node>,
        right: Box<Node>,
        quantifier: SetQuantifier,
        left_output: RowType,
        right_output: RowType,
        output: RowType,
    },
    Aggregate {
        input: Box<Node>,
        groups: Vec<BoundGroupTerm>,
        terms: Vec<BoundAggregateTerm>,
    },
}

impl Plan {
    pub(crate) fn walk(&self, visitor: &mut impl FnMut(&Node)) {
        for binding in &self.bindings {
            match &binding.kind {
                BindingPlanKind::Derived { plan, .. } => plan.walk(visitor),
                BindingPlanKind::Recursive { anchor, step, .. } => {
                    anchor.walk(visitor);
                    step.walk(visitor);
                }
            }
        }
        self.root.walk(visitor);
    }

    pub(super) fn walk_mut(&mut self, visitor: &mut impl FnMut(&mut Node)) {
        for binding in &mut self.bindings {
            match &mut binding.kind {
                BindingPlanKind::Derived { plan, .. } => plan.walk_mut(visitor),
                BindingPlanKind::Recursive { anchor, step, .. } => {
                    anchor.walk_mut(visitor);
                    step.walk_mut(visitor);
                }
            }
        }
        self.root.walk_mut(visitor);
    }
}

impl Plan {
    /// Canonical structural identity of the chosen physical plan: operator
    /// shapes, access paths, join order, and binding strategies. Literal
    /// values (keys, range bounds) are excluded — plan identity is the shape
    /// that executed, not the parameters it ran with.
    pub fn fingerprint(&self) -> lir::fingerprint::Fingerprint {
        use lir::fingerprint as fp;

        let positions: std::collections::HashMap<&str, u32> = self
            .bindings
            .iter()
            .enumerate()
            .map(|(position, binding)| (binding.name.as_str(), position as u32))
            .collect();
        let mut payload = Vec::new();
        payload.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match &binding.kind {
                BindingPlanKind::Derived { plan, strategy } => {
                    payload.push(1);
                    payload.push(match strategy {
                        BindingStrategy::Materialize => 1,
                        BindingStrategy::Replay => 2,
                    });
                    encode_plan_node(plan, &positions, &mut payload);
                }
                BindingPlanKind::Recursive {
                    anchor,
                    step,
                    accumulation,
                    ..
                } => {
                    payload.push(2);
                    payload.push(fp::accumulation_byte(*accumulation));
                    encode_plan_node(anchor, &positions, &mut payload);
                    encode_plan_node(step, &positions, &mut payload);
                }
            }
        }
        encode_plan_node(&self.root, &positions, &mut payload);
        payload.push(fp::cardinality_byte(self.cardinality));
        fp::finish(fp::DOMAIN_PLAN, &payload)
    }
}

pub(super) fn scan_schema_id(scan: &bound::Relation) -> u32 {
    match &scan.node {
        bound::RelationNode::Scan { table, .. } => table.schema_id.get(),
        _ => 0,
    }
}

impl Plan {
    /// The table this plan reads, when it reads exactly one. An estimate
    /// for a single-table plan can be grounded by that table's synopsis;
    /// a multi-table plan cannot without join estimation.
    pub fn sole_scanned_table(&self) -> Option<crate::engine::catalog::identity::SchemaId> {
        match self.root.scanned_tables().as_slice() {
            [table] => crate::engine::catalog::identity::SchemaId::new(*table).ok(),
            _ => None,
        }
    }
}

impl Node {
    /// Stable logical identities of every table this plan subtree scans.
    pub(super) fn scanned_tables(&self) -> Vec<u32> {
        let mut tables = Vec::new();
        self.walk(&mut |node| {
            let scan = match &node.kind {
                NodeKind::PrimaryKeyGet { scan, .. }
                | NodeKind::TableScan { scan, .. }
                | NodeKind::IndexRangeScan { scan, .. } => Some(scan),
                _ => None,
            };
            if let Some(scan) = scan {
                let schema = scan_schema_id(scan);
                if schema != 0 && !tables.contains(&schema) {
                    tables.push(schema);
                }
            }
        });
        tables
    }
}

fn encode_str(value: &str, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&(value.len() as u64).to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
}

fn encode_plan_node(
    node: &Node,
    positions: &std::collections::HashMap<&str, u32>,
    payload: &mut Vec<u8>,
) {
    use lir::fingerprint as fp;

    match &node.kind {
        NodeKind::PrimaryKeyGet { scan, key, .. } => {
            payload.push(1);
            payload.extend_from_slice(&scan_schema_id(scan).to_be_bytes());
            payload.extend_from_slice(&(key.len() as u64).to_be_bytes());
        }
        NodeKind::TableScan { scan, .. } => {
            payload.push(2);
            payload.extend_from_slice(&scan_schema_id(scan).to_be_bytes());
        }
        NodeKind::Rows(relation) => {
            payload.push(3);
            payload.extend_from_slice(&(relation.output().fields.len() as u64).to_be_bytes());
        }
        NodeKind::IndexRangeScan {
            scan,
            index,
            equality_prefix,
            range,
            ..
        } => {
            payload.push(4);
            payload.extend_from_slice(&scan_schema_id(scan).to_be_bytes());
            encode_str(index.logical_id.as_str(), payload);
            payload.extend_from_slice(&(equality_prefix.len() as u64).to_be_bytes());
            match range {
                Some(range) => {
                    payload.push(1);
                    encode_str(&range.column, payload);
                    payload.push(u8::from(range.lower.is_some()));
                    payload.push(u8::from(range.upper.is_some()));
                }
                None => payload.push(0),
            }
        }
        NodeKind::Filter { input, .. } => {
            payload.push(5);
            encode_plan_node(input, positions, payload);
        }
        NodeKind::Attach {
            input,
            specifications,
        } => {
            payload.push(6);
            payload.extend_from_slice(&(specifications.len() as u64).to_be_bytes());
            for specification in specifications {
                payload.push(match specification.kind {
                    CrossingKind::Exists => 1,
                    CrossingKind::First => 2,
                    CrossingKind::Scalar => 3,
                    CrossingKind::Array => 4,
                });
                encode_plan_node(&specification.plan, positions, payload);
            }
            encode_plan_node(input, positions, payload);
        }
        NodeKind::Project { input, fields } => {
            payload.push(7);
            payload.extend_from_slice(&(fields.len() as u64).to_be_bytes());
            encode_plan_node(input, positions, payload);
        }
        NodeKind::Sort { input, terms } => {
            payload.push(8);
            payload.extend_from_slice(&(terms.len() as u64).to_be_bytes());
            for term in terms {
                payload.push(u8::from(term.descending));
            }
            encode_plan_node(input, positions, payload);
        }
        NodeKind::Slice {
            input,
            offset,
            limit,
        } => {
            payload.push(9);
            payload.extend_from_slice(&(*offset as u64).to_be_bytes());
            match limit {
                Some(limit) => {
                    payload.push(1);
                    payload.extend_from_slice(&(*limit as u64).to_be_bytes());
                }
                None => payload.push(0),
            }
            encode_plan_node(input, positions, payload);
        }
        NodeKind::Reference { binding, .. } => {
            payload.push(10);
            let position = positions.get(binding.as_str()).copied().unwrap_or(u32::MAX);
            payload.extend_from_slice(&position.to_be_bytes());
        }
        NodeKind::RecursiveReference { binding, .. } => {
            payload.push(11);
            let position = positions.get(binding.as_str()).copied().unwrap_or(u32::MAX);
            payload.extend_from_slice(&position.to_be_bytes());
        }
        NodeKind::Distinct { input, .. } => {
            payload.push(12);
            encode_plan_node(input, positions, payload);
        }
        NodeKind::NestedLoopJoin {
            left, right, kind, ..
        } => {
            payload.push(13);
            payload.push(fp::join_byte(*kind));
            encode_plan_node(left, positions, payload);
            encode_plan_node(right, positions, payload);
        }
        NodeKind::HashJoin {
            left,
            right,
            kind,
            keys,
            memory_limit_bytes,
            ..
        } => {
            payload.push(18);
            payload.push(fp::join_byte(*kind));
            payload.extend_from_slice(&(keys.len() as u64).to_be_bytes());
            payload.extend_from_slice(&memory_limit_bytes.to_be_bytes());
            encode_plan_node(left, positions, payload);
            encode_plan_node(right, positions, payload);
        }
        NodeKind::IndexedLookupJoin {
            left,
            right,
            kind,
            keys,
            ..
        } => {
            payload.push(19);
            payload.push(fp::join_byte(*kind));
            payload.extend_from_slice(&(keys.len() as u64).to_be_bytes());
            encode_plan_node(left, positions, payload);
            encode_plan_node(right, positions, payload);
        }
        NodeKind::JoinGraphChoice { input, decision } => {
            payload.push(20);
            payload.extend_from_slice(&(decision.input_count as u64).to_be_bytes());
            payload.extend_from_slice(&(decision.edge_count as u64).to_be_bytes());
            payload.extend_from_slice(&(decision.root_input as u64).to_be_bytes());
            encode_plan_node(input, positions, payload);
        }
        NodeKind::ShreddedYannakakisJoin {
            inputs,
            edges,
            root_input,
            memory_limit_bytes,
            ..
        } => {
            payload.push(21);
            payload.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(edges.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(*root_input as u64).to_be_bytes());
            payload.extend_from_slice(&memory_limit_bytes.to_be_bytes());
            for edge in edges {
                payload.extend_from_slice(&(edge.parent as u64).to_be_bytes());
                payload.extend_from_slice(&(edge.child as u64).to_be_bytes());
                payload.extend_from_slice(&(edge.keys.len() as u64).to_be_bytes());
            }
            for input in inputs {
                encode_plan_node(input, positions, payload);
            }
        }
        NodeKind::PredicateTransferJoin {
            inputs,
            schedule,
            join_plan,
            bits_per_key,
            hash_functions,
            runtime_policy,
            memory_limit_bytes,
            ..
        } => {
            payload.push(22);
            payload.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(schedule.forward.edges.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(schedule.backward.edges.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(schedule.pruned_paths as u64).to_be_bytes());
            payload.extend_from_slice(&(schedule.root_input as u64).to_be_bytes());
            payload.push(*bits_per_key);
            payload.push(*hash_functions);
            payload.extend_from_slice(&runtime_policy.block_rows.to_be_bytes());
            payload.extend_from_slice(&runtime_policy.build_sample_rows.to_be_bytes());
            payload
                .extend_from_slice(&runtime_policy.build_selectivity_threshold_bps.to_be_bytes());
            payload.extend_from_slice(&runtime_policy.build_progress_threshold_bps.to_be_bytes());
            payload.extend_from_slice(&runtime_policy.probe_sample_rows.to_be_bytes());
            payload.extend_from_slice(&runtime_policy.probe_stop_threshold_bps.to_be_bytes());
            payload.extend_from_slice(&memory_limit_bytes.to_be_bytes());
            for pass in [&schedule.forward, &schedule.backward] {
                payload.extend_from_slice(&(pass.order.len() as u64).to_be_bytes());
                for input in &pass.order {
                    payload.extend_from_slice(&(*input as u64).to_be_bytes());
                }
                for edge in &pass.edges {
                    payload.extend_from_slice(&(edge.source as u64).to_be_bytes());
                    payload.extend_from_slice(&(edge.target as u64).to_be_bytes());
                    payload.extend_from_slice(&(edge.keys.len() as u64).to_be_bytes());
                }
            }
            for input in inputs {
                encode_plan_node(input, positions, payload);
            }
            encode_plan_node(join_plan, positions, payload);
        }
        NodeKind::PredicateTransferInput { input, .. } => {
            payload.push(23);
            payload.extend_from_slice(&(*input as u64).to_be_bytes());
        }
        NodeKind::Concatenate { inputs, .. } => {
            payload.push(14);
            payload.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
            for input in inputs {
                encode_plan_node(input, positions, payload);
            }
        }
        NodeKind::Intersect {
            left,
            right,
            quantifier,
            ..
        } => {
            payload.push(15);
            payload.push(fp::quantifier_byte(*quantifier));
            encode_plan_node(left, positions, payload);
            encode_plan_node(right, positions, payload);
        }
        NodeKind::Except {
            left,
            right,
            quantifier,
            ..
        } => {
            payload.push(16);
            payload.push(fp::quantifier_byte(*quantifier));
            encode_plan_node(left, positions, payload);
            encode_plan_node(right, positions, payload);
        }
        NodeKind::Aggregate {
            input,
            groups,
            terms,
        } => {
            payload.push(17);
            payload.extend_from_slice(&(groups.len() as u64).to_be_bytes());
            payload.extend_from_slice(&(terms.len() as u64).to_be_bytes());
            for term in terms {
                payload.push(fp::aggregate_byte(term.function));
            }
            encode_plan_node(input, positions, payload);
        }
    }
}

impl Node {
    pub fn children(&self) -> Vec<&Self> {
        match &self.kind {
            NodeKind::PrimaryKeyGet { .. }
            | NodeKind::TableScan { .. }
            | NodeKind::Rows(_)
            | NodeKind::IndexRangeScan { .. }
            | NodeKind::Reference { .. }
            | NodeKind::RecursiveReference { .. }
            | NodeKind::PredicateTransferInput { .. } => Vec::new(),
            NodeKind::Filter { input, .. }
            | NodeKind::Project { input, .. }
            | NodeKind::Sort { input, .. }
            | NodeKind::Slice { input, .. }
            | NodeKind::Distinct { input, .. }
            | NodeKind::Aggregate { input, .. }
            | NodeKind::JoinGraphChoice { input, .. } => vec![input],
            NodeKind::Attach {
                input,
                specifications,
            } => specifications
                .iter()
                .map(|specification| &specification.plan)
                .chain(std::iter::once(&**input))
                .collect(),
            NodeKind::NestedLoopJoin { left, right, .. }
            | NodeKind::HashJoin { left, right, .. }
            | NodeKind::IndexedLookupJoin { left, right, .. }
            | NodeKind::Intersect { left, right, .. }
            | NodeKind::Except { left, right, .. } => vec![left, right],
            NodeKind::Concatenate { inputs, .. }
            | NodeKind::ShreddedYannakakisJoin { inputs, .. }
            | NodeKind::PredicateTransferJoin { inputs, .. } => inputs.iter().collect(),
        }
    }

    pub fn walk(&self, visitor: &mut impl FnMut(&Self)) {
        visitor(self);
        for child in self.children() {
            child.walk(visitor);
        }
    }

    fn children_mut(&mut self) -> Vec<&mut Self> {
        match &mut self.kind {
            NodeKind::PrimaryKeyGet { .. }
            | NodeKind::TableScan { .. }
            | NodeKind::Rows(_)
            | NodeKind::IndexRangeScan { .. }
            | NodeKind::Reference { .. }
            | NodeKind::RecursiveReference { .. }
            | NodeKind::PredicateTransferInput { .. } => Vec::new(),
            NodeKind::Filter { input, .. }
            | NodeKind::Project { input, .. }
            | NodeKind::Sort { input, .. }
            | NodeKind::Slice { input, .. }
            | NodeKind::Distinct { input, .. }
            | NodeKind::Aggregate { input, .. }
            | NodeKind::JoinGraphChoice { input, .. } => vec![input],
            NodeKind::Attach {
                input,
                specifications,
            } => std::iter::once(&mut **input)
                .chain(
                    specifications
                        .iter_mut()
                        .map(|specification| &mut specification.plan),
                )
                .collect(),
            NodeKind::NestedLoopJoin { left, right, .. }
            | NodeKind::HashJoin { left, right, .. }
            | NodeKind::IndexedLookupJoin { left, right, .. }
            | NodeKind::Intersect { left, right, .. }
            | NodeKind::Except { left, right, .. } => vec![left, right],
            NodeKind::Concatenate { inputs, .. }
            | NodeKind::ShreddedYannakakisJoin { inputs, .. }
            | NodeKind::PredicateTransferJoin { inputs, .. } => inputs.iter_mut().collect(),
        }
    }

    pub(super) fn walk_mut(&mut self, visitor: &mut impl FnMut(&mut Self)) {
        visitor(self);
        for child in self.children_mut() {
            child.walk_mut(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{BinaryOp, RootCardinality, SlotId, Value};
    use crate::engine::planner::analysis::{Correlation, CorrelationKind};
    use crate::engine::planner::test_support::{column, query, scan};
    use crate::engine::planner::{PlanOptions, plan_query};

    use super::*;

    #[test]
    fn plan_fingerprint_tracks_shape_and_access_path_not_literals() {
        let filtered = |value: &str| {
            let scan = scan();
            let predicate = bound::Expr::binary(
                BinaryOp::Eq,
                column(&scan, "board_id"),
                bound::Expr::literal(Value::Text(value.into())),
            );
            query(bound::Relation::filter(scan, predicate), 3)
        };

        let indexed = plan_query(&filtered("b1"), PlanOptions::default());
        let repeated = plan_query(&filtered("b1"), PlanOptions::default());
        assert_eq!(indexed.fingerprint(), repeated.fingerprint());

        let other_literal = plan_query(&filtered("b2"), PlanOptions::default());
        assert_eq!(indexed.fingerprint(), other_literal.fingerprint());

        let forced_scan = plan_query(
            &filtered("b1"),
            PlanOptions {
                full_scan_only: true,
                ..PlanOptions::default()
            },
        );
        assert_ne!(indexed.fingerprint(), forced_scan.fingerprint());
    }

    fn leaf(scan: &bound::Relation) -> Node {
        NodeKind::TableScan {
            scan: Box::new(scan.clone()),
            decode_columns: Vec::new(),
            access: AccessDecision::default(),
        }
        .bare()
    }

    #[test]
    fn plan_walkers_cover_bindings_root_and_attached_plans() {
        let scan = scan();
        let output = scan.output().clone();
        let mut plan = Plan {
            bindings: vec![BindingPlan {
                name: "saved".into(),
                output: output.clone(),
                sensitive: false,
                kind: BindingPlanKind::Derived {
                    plan: Box::new(leaf(&scan)),
                    strategy: BindingStrategy::Replay,
                },
            }],
            root: NodeKind::Attach {
                input: Box::new(leaf(&scan)),
                specifications: vec![AttachSpec {
                    slot: SlotId(3),
                    kind: CrossingKind::Exists,
                    correlation: Correlation {
                        kind: CorrelationKind::Uncorrelated,
                        keys: Vec::new(),
                    },
                    plan: leaf(&scan),
                    output: output.clone(),
                }],
            }
            .bare(),
            cardinality: RootCardinality::Many,
            output,
            dependencies: CatalogDependencies::default(),
            next_slot: SlotId(4),
            memo: crate::engine::planner::memo::MemoReport::default(),
        };

        let mut immutable = 0;
        plan.walk(&mut |_| immutable += 1);
        let mut mutable = 0;
        plan.walk_mut(&mut |_| mutable += 1);

        assert_eq!(immutable, 4);
        assert_eq!(mutable, immutable);
    }
}
