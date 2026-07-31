//! Executor-facing physical operator tree.

use serde::{Deserialize, Serialize};

use crate::engine::catalog::model::{CatalogDependencies, Column, Index};
use crate::engine::lir::bound::{self, BoundAggregateTerm, BoundGroupTerm, BoundOrderTerm};
use crate::engine::lir::{
    self, RecursiveAccumulation, RootCardinality, RowType, SetQuantifier, SlotId,
};

use super::analysis::{ConstValue, Correlation, RangeBound};

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub bindings: Vec<BindingPlan>,
    pub root: Node,
    pub cardinality: RootCardinality,
    pub output: RowType,
    pub dependencies: CatalogDependencies,
    pub next_slot: SlotId,
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AccessDecision {
    pub candidates: Vec<AccessCandidate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccessCandidate {
    pub method: String,
    pub score: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chosen: bool,
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

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
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
        right_output: RowType,
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
    pub(super) fn walk(&self, visitor: &mut impl FnMut(&Node)) {
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

impl Node {
    pub fn children(&self) -> Vec<&Self> {
        match self {
            Self::PrimaryKeyGet { .. }
            | Self::TableScan { .. }
            | Self::Rows(_)
            | Self::IndexRangeScan { .. }
            | Self::Reference { .. }
            | Self::RecursiveReference { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Sort { input, .. }
            | Self::Slice { input, .. }
            | Self::Distinct { input, .. }
            | Self::Aggregate { input, .. } => vec![input],
            Self::Attach {
                input,
                specifications,
            } => specifications
                .iter()
                .map(|specification| &specification.plan)
                .chain(std::iter::once(&**input))
                .collect(),
            Self::NestedLoopJoin { left, right, .. }
            | Self::Intersect { left, right, .. }
            | Self::Except { left, right, .. } => vec![left, right],
            Self::Concatenate { inputs, .. } => inputs.iter().collect(),
        }
    }

    pub fn walk(&self, visitor: &mut impl FnMut(&Self)) {
        visitor(self);
        for child in self.children() {
            child.walk(visitor);
        }
    }

    fn children_mut(&mut self) -> Vec<&mut Self> {
        match self {
            Self::PrimaryKeyGet { .. }
            | Self::TableScan { .. }
            | Self::Rows(_)
            | Self::IndexRangeScan { .. }
            | Self::Reference { .. }
            | Self::RecursiveReference { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Sort { input, .. }
            | Self::Slice { input, .. }
            | Self::Distinct { input, .. }
            | Self::Aggregate { input, .. } => vec![input],
            Self::Attach {
                input,
                specifications,
            } => std::iter::once(&mut **input)
                .chain(
                    specifications
                        .iter_mut()
                        .map(|specification| &mut specification.plan),
                )
                .collect(),
            Self::NestedLoopJoin { left, right, .. }
            | Self::Intersect { left, right, .. }
            | Self::Except { left, right, .. } => vec![left, right],
            Self::Concatenate { inputs, .. } => inputs.iter_mut().collect(),
        }
    }

    fn walk_mut(&mut self, visitor: &mut impl FnMut(&mut Self)) {
        visitor(self);
        for child in self.children_mut() {
            child.walk_mut(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::lir::{RootCardinality, SlotId};
    use crate::engine::planner::analysis::{Correlation, CorrelationKind};
    use crate::engine::planner::test_support::scan;

    use super::*;

    fn leaf(scan: &bound::Relation) -> Node {
        Node::TableScan {
            scan: Box::new(scan.clone()),
            decode_columns: Vec::new(),
            access: AccessDecision::default(),
        }
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
            root: Node::Attach {
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
            },
            cardinality: RootCardinality::Many,
            output,
            dependencies: CatalogDependencies::default(),
            next_slot: SlotId(4),
        };

        let mut immutable = 0;
        plan.walk(&mut |_| immutable += 1);
        let mut mutable = 0;
        plan.walk_mut(&mut |_| mutable += 1);

        assert_eq!(immutable, 4);
        assert_eq!(mutable, immutable);
    }
}
