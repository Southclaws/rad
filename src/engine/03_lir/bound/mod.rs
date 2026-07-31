//! Slot-addressed LIR produced by `04_planner` binding.

mod text_pattern;

use crate::engine::catalog::model::Table;

use super::{
    AggregateFunction, BinaryOp, Cardinality, Field, JoinKind, Kind, RecursiveAccumulation,
    RootCardinality, RowType, SetQuantifier, SlotId, TextComparison, TextMatchPart, Type,
    UNBOUNDED, UnaryOp, Value,
};

pub use text_pattern::{TextPattern, TextPatternError};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlotSet {
    words: Vec<u64>,
}

impl SlotSet {
    pub fn new(ids: impl IntoIterator<Item = SlotId>) -> Self {
        let mut value = Self::default();
        for id in ids {
            let word = id.0 / 64;
            value.words.resize(value.words.len().max(word + 1), 0);
            value.words[word] |= 1 << (id.0 % 64);
        }
        value
    }

    pub fn contains(&self, id: SlotId) -> bool {
        let word = id.0 / 64;
        self.words
            .get(word)
            .is_some_and(|value| value & (1 << (id.0 % 64)) != 0)
    }

    pub fn union(&self, other: &Self) -> Self {
        let mut words = vec![0; self.words.len().max(other.words.len())];
        for (index, word) in self.words.iter().enumerate() {
            words[index] |= word;
        }
        for (index, word) in other.words.iter().enumerate() {
            words[index] |= word;
        }
        Self { words }.compacted()
    }

    pub fn without(&self, other: &Self) -> Self {
        let mut words = self.words.clone();
        for (index, word) in other.words.iter().enumerate() {
            if let Some(value) = words.get_mut(index) {
                *value &= !word;
            }
        }
        Self { words }.compacted()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    pub fn slots(&self) -> Vec<SlotId> {
        let mut slots = Vec::new();
        for (word_index, word) in self.words.iter().copied().enumerate() {
            for bit in 0..64 {
                if word & (1 << bit) != 0 {
                    slots.push(SlotId(word_index * 64 + bit));
                }
            }
        }
        slots
    }

    fn compacted(mut self) -> Self {
        while self.words.last() == Some(&0) {
            self.words.pop();
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BranchArm {
    pub when: Expr,
    pub then: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    SlotRef {
        slot: SlotId,
        name: String,
        value_type: Type,
    },
    Unary {
        op: UnaryOp,
        expression: Box<Expr>,
        value_type: Type,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
        value_type: Type,
    },
    Cast {
        expression: Box<Expr>,
        to: Kind,
        value_type: Type,
    },
    Branch {
        arms: Vec<BranchArm>,
        otherwise: Box<Expr>,
        value_type: Type,
    },
    TextMatch {
        value: Box<Expr>,
        pattern: TextPattern,
        value_type: Type,
    },
    Exists(Box<Relation>),
    First {
        relation: Box<Relation>,
        value_type: Type,
    },
    Scalar {
        relation: Box<Relation>,
        value_type: Type,
    },
    Array {
        relation: Box<Relation>,
        value_type: Type,
    },
}

impl Expr {
    pub fn literal(value: Value) -> Self {
        Self::Literal(value)
    }

    pub fn slot(slot: impl Into<SlotId>, name: impl Into<String>, value_type: Type) -> Self {
        Self::SlotRef {
            slot: slot.into(),
            name: name.into(),
            value_type,
        }
    }

    pub fn unary(op: UnaryOp, expression: Expr) -> Self {
        let value_type = match op {
            UnaryOp::Not => Type::scalar(Kind::Bool, expression.value_type().nullable),
            UnaryOp::Negate => expression.value_type(),
            UnaryOp::IsNull | UnaryOp::IsNotNull => Type::scalar(Kind::Bool, false),
        };
        Self::Unary {
            op,
            expression: Box::new(expression),
            value_type,
        }
    }

    pub fn binary(op: BinaryOp, left: Expr, right: Expr) -> Self {
        let nullable = left.value_type().nullable || right.value_type().nullable;
        let kind = match op {
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Lte
            | BinaryOp::Gt
            | BinaryOp::Gte
            | BinaryOp::And
            | BinaryOp::Or => Kind::Bool,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                if left.value_type().kind == Kind::Float64
                    || right.value_type().kind == Kind::Float64
                {
                    Kind::Float64
                } else {
                    Kind::Int64
                }
            }
        };
        Self::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            value_type: Type::scalar(kind, nullable),
        }
    }

    pub fn cast(expression: Expr, to: Kind) -> Self {
        let nullable = expression.value_type().nullable;
        Self::Cast {
            expression: Box::new(expression),
            to,
            value_type: Type::scalar(to, nullable),
        }
    }

    pub fn branch(arms: Vec<BranchArm>, otherwise: Expr) -> Self {
        let mut value_type = otherwise.value_type();
        value_type.nullable |= arms.iter().any(|arm| arm.then.value_type().nullable);
        Self::Branch {
            arms,
            otherwise: Box::new(otherwise),
            value_type,
        }
    }

    pub fn text_match(
        value: Expr,
        parts: &[TextMatchPart],
        comparison: TextComparison,
    ) -> Result<Self, TextPatternError> {
        let nullable = value.value_type().nullable;
        Ok(Self::TextMatch {
            value: Box::new(value),
            pattern: TextPattern::compile(parts, comparison)?,
            value_type: Type::scalar(Kind::Bool, nullable),
        })
    }

    pub fn exists(relation: Relation) -> Self {
        Self::Exists(Box::new(relation))
    }

    pub fn first(relation: Relation) -> Self {
        let value_type = Type::row(relation.output().clone(), true);
        Self::First {
            relation: Box::new(relation),
            value_type,
        }
    }

    pub fn scalar(relation: Relation) -> Result<Self, BoundError> {
        let [field] = relation.output().fields.as_slice() else {
            return Err(BoundError::ScalarArity(relation.output().fields.len()));
        };
        let value_type = field.value_type.clone().with_nullable(true);
        Ok(Self::Scalar {
            relation: Box::new(relation),
            value_type,
        })
    }

    pub fn array(relation: Relation) -> Self {
        let value_type = Type::array(Type::row(relation.output().clone(), false));
        Self::Array {
            relation: Box::new(relation),
            value_type,
        }
    }

    pub fn value_type(&self) -> Type {
        match self {
            Self::Literal(value) => Type::catalog_scalar(value.scalar_type(), value.is_null()),
            Self::SlotRef { value_type, .. }
            | Self::Unary { value_type, .. }
            | Self::Binary { value_type, .. }
            | Self::Cast { value_type, .. }
            | Self::Branch { value_type, .. }
            | Self::TextMatch { value_type, .. }
            | Self::First { value_type, .. }
            | Self::Scalar { value_type, .. }
            | Self::Array { value_type, .. } => value_type.clone(),
            Self::Exists(_) => Type::scalar(Kind::Bool, false),
        }
    }

    pub fn free_slots(&self) -> SlotSet {
        match self {
            Self::Literal(_) => SlotSet::default(),
            Self::SlotRef { slot, .. } => SlotSet::new([*slot]),
            Self::Unary { expression, .. } | Self::Cast { expression, .. } => {
                expression.free_slots()
            }
            Self::Binary { left, right, .. } => left.free_slots().union(&right.free_slots()),
            Self::Branch {
                arms, otherwise, ..
            } => arms.iter().fold(otherwise.free_slots(), |slots, arm| {
                slots
                    .union(&arm.when.free_slots())
                    .union(&arm.then.free_slots())
            }),
            Self::TextMatch { value, .. } => value.free_slots(),
            Self::Exists(relation)
            | Self::First { relation, .. }
            | Self::Scalar { relation, .. }
            | Self::Array { relation, .. } => relation.free_slots().clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Laws {
    output: RowType,
    free: SlotSet,
    produced: SlotSet,
    cardinality: Cardinality,
    ordered: bool,
}

impl Laws {
    fn leaf(output: RowType, cardinality: Cardinality) -> Self {
        let produced = SlotSet::new(output.slots());
        Self {
            output,
            free: SlotSet::default(),
            produced,
            cardinality,
            ordered: false,
        }
    }

    fn inherited(input: &Relation) -> Self {
        Self {
            output: input.output().clone(),
            free: input.free_slots().clone(),
            produced: input.produced().clone(),
            cardinality: input.cardinality(),
            ordered: input.is_ordered(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectField {
    pub name: String,
    pub slot: SlotId,
    pub expression: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundGroupTerm {
    pub name: String,
    pub slot: SlotId,
    pub expression: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundAggregateTerm {
    pub function: AggregateFunction,
    pub argument: Option<Expr>,
    pub name: String,
    pub slot: SlotId,
    pub value_type: Type,
}

pub fn aggregate_term_type(function: AggregateFunction, argument: Option<&Expr>) -> Type {
    match function {
        AggregateFunction::Count => Type::scalar(Kind::Int64, false),
        AggregateFunction::Average => Type::scalar(Kind::Float64, true),
        AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => argument
            .expect("non-count aggregate requires an argument")
            .value_type()
            .with_nullable(true),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoundOrderTerm {
    pub expression: Expr,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RelationNode {
    Scan {
        table: Table,
        scope: String,
    },
    Rows {
        scope: String,
        values: Vec<Vec<Value>>,
    },
    Filter {
        input: Box<Relation>,
        predicate: Expr,
    },
    Project {
        input: Box<Relation>,
        scope: String,
        fields: Vec<ProjectField>,
    },
    Join {
        left: Box<Relation>,
        right: Box<Relation>,
        kind: JoinKind,
        on: Expr,
    },
    Concatenate {
        inputs: Vec<Relation>,
        scope: String,
    },
    Intersect {
        left: Box<Relation>,
        right: Box<Relation>,
        quantifier: SetQuantifier,
        scope: String,
    },
    Except {
        left: Box<Relation>,
        right: Box<Relation>,
        quantifier: SetQuantifier,
        scope: String,
    },
    Aggregate {
        input: Box<Relation>,
        groups: Vec<BoundGroupTerm>,
        terms: Vec<BoundAggregateTerm>,
    },
    Order {
        input: Box<Relation>,
        terms: Vec<BoundOrderTerm>,
    },
    Slice {
        input: Box<Relation>,
        offset: usize,
        limit: Option<usize>,
    },
    Ref {
        binding: String,
        scope: String,
        canonical: Vec<SlotId>,
    },
    RecursiveRef {
        binding: String,
        scope: String,
        canonical: Vec<SlotId>,
    },
    Distinct(Box<Relation>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub node: RelationNode,
    laws: Laws,
}

impl Relation {
    pub fn scan(table: Table, scope: impl Into<String>, slots: Vec<SlotId>) -> Self {
        assert_eq!(table.columns.len(), slots.len());
        let output = RowType {
            fields: table
                .columns
                .iter()
                .zip(slots)
                .map(|(column, slot)| Field {
                    name: column.name.clone(),
                    slot,
                    value_type: Type::catalog_scalar(column.scalar_type, column.nullable),
                })
                .collect(),
        };
        Self {
            laws: Laws::leaf(output, Cardinality::MANY),
            node: RelationNode::Scan {
                table,
                scope: scope.into(),
            },
        }
    }

    pub fn rows(scope: impl Into<String>, fields: Vec<Field>, values: Vec<Vec<Value>>) -> Self {
        let count = i64::try_from(values.len()).unwrap_or(i64::MAX);
        Self {
            laws: Laws::leaf(
                RowType { fields },
                Cardinality {
                    min: count,
                    max: count,
                },
            ),
            node: RelationNode::Rows {
                scope: scope.into(),
                values,
            },
        }
    }

    pub fn filter(input: Relation, predicate: Expr) -> Self {
        let mut laws = Laws::inherited(&input);
        laws.free = laws
            .free
            .union(&predicate.free_slots().without(input.produced()));
        laws.cardinality.min = 0;
        Self {
            node: RelationNode::Filter {
                input: Box::new(input),
                predicate,
            },
            laws,
        }
    }

    pub fn project(input: Relation, scope: impl Into<String>, fields: Vec<ProjectField>) -> Self {
        let output = RowType {
            fields: fields
                .iter()
                .map(|field| Field {
                    name: field.name.clone(),
                    slot: field.slot,
                    value_type: field.expression.value_type(),
                })
                .collect(),
        };
        let free = fields
            .iter()
            .fold(input.free_slots().clone(), |free, field| {
                free.union(&field.expression.free_slots().without(input.produced()))
            });
        let laws = Laws {
            output: output.clone(),
            free,
            produced: input.produced().union(&SlotSet::new(output.slots())),
            cardinality: input.cardinality(),
            ordered: input.is_ordered(),
        };
        Self {
            node: RelationNode::Project {
                input: Box::new(input),
                scope: scope.into(),
                fields,
            },
            laws,
        }
    }

    pub fn join(left: Relation, right: Relation, kind: JoinKind, on: Expr) -> Self {
        let mut fields = left.output().fields.clone();
        fields.extend(right.output().fields.iter().cloned().map(|mut field| {
            if kind == JoinKind::Left {
                field.value_type.nullable = true;
            }
            field
        }));
        let produced = left.produced().union(right.produced());
        let free = left
            .free_slots()
            .union(right.free_slots())
            .union(&on.free_slots().without(&produced));
        let left_card = left.cardinality();
        let right_card = right.cardinality();
        let mut cardinality = Cardinality::MANY;
        if kind == JoinKind::Left {
            cardinality.min = left_card.min;
        }
        if left_card.max != UNBOUNDED && right_card.max != UNBOUNDED {
            let right_max = if kind == JoinKind::Left {
                right_card.max.max(1)
            } else {
                right_card.max
            };
            cardinality.max = left_card.max.saturating_mul(right_max);
        }
        Self {
            laws: Laws {
                output: RowType { fields },
                free,
                produced,
                cardinality,
                ordered: false,
            },
            node: RelationNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
            },
        }
    }

    pub fn concatenate(
        inputs: Vec<Relation>,
        scope: impl Into<String>,
        fields: Vec<Field>,
    ) -> Self {
        let output = RowType { fields };
        let mut free = SlotSet::default();
        let mut produced = SlotSet::default();
        let mut cardinality = Cardinality { min: 0, max: 0 };
        for input in &inputs {
            free = free.union(input.free_slots());
            produced = produced.union(input.produced());
            cardinality.min = cardinality.min.saturating_add(input.cardinality().min);
            cardinality.max =
                if cardinality.max == UNBOUNDED || input.cardinality().max == UNBOUNDED {
                    UNBOUNDED
                } else {
                    cardinality.max.saturating_add(input.cardinality().max)
                };
        }
        Self {
            laws: Laws {
                produced: produced.union(&SlotSet::new(output.slots())),
                output,
                free,
                cardinality,
                ordered: false,
            },
            node: RelationNode::Concatenate {
                inputs,
                scope: scope.into(),
            },
        }
    }

    pub fn intersect(
        left: Relation,
        right: Relation,
        quantifier: SetQuantifier,
        scope: impl Into<String>,
        fields: Vec<Field>,
    ) -> Self {
        let upper = match (left.cardinality().max, right.cardinality().max) {
            (UNBOUNDED, right) => right,
            (left, UNBOUNDED) => left,
            (left, right) => left.min(right),
        };
        let laws = Self::set_operation_laws(&left, &right, fields, upper);
        Self {
            node: RelationNode::Intersect {
                left: Box::new(left),
                right: Box::new(right),
                quantifier,
                scope: scope.into(),
            },
            laws,
        }
    }

    pub fn except(
        left: Relation,
        right: Relation,
        quantifier: SetQuantifier,
        scope: impl Into<String>,
        fields: Vec<Field>,
    ) -> Self {
        let upper = left.cardinality().max;
        let laws = Self::set_operation_laws(&left, &right, fields, upper);
        Self {
            node: RelationNode::Except {
                left: Box::new(left),
                right: Box::new(right),
                quantifier,
                scope: scope.into(),
            },
            laws,
        }
    }

    fn set_operation_laws(
        left: &Relation,
        right: &Relation,
        fields: Vec<Field>,
        maximum: i64,
    ) -> Laws {
        let output = RowType { fields };
        let produced = left
            .produced()
            .union(right.produced())
            .union(&SlotSet::new(output.slots()));
        Laws {
            output,
            free: left.free_slots().union(right.free_slots()),
            produced,
            cardinality: Cardinality {
                min: 0,
                max: maximum,
            },
            ordered: false,
        }
    }

    pub fn aggregate(
        input: Relation,
        groups: Vec<BoundGroupTerm>,
        terms: Vec<BoundAggregateTerm>,
    ) -> Self {
        let mut fields = Vec::with_capacity(groups.len() + terms.len());
        let mut free = input.free_slots().clone();
        for group in &groups {
            fields.push(Field {
                name: group.name.clone(),
                slot: group.slot,
                value_type: group.expression.value_type(),
            });
            free = free.union(&group.expression.free_slots().without(input.produced()));
        }
        for term in &terms {
            fields.push(Field {
                name: term.name.clone(),
                slot: term.slot,
                value_type: term.value_type.clone(),
            });
            if let Some(argument) = &term.argument {
                free = free.union(&argument.free_slots().without(input.produced()));
            }
        }
        let output = RowType { fields };
        let cardinality = if groups.is_empty() {
            Cardinality { min: 1, max: 1 }
        } else {
            Cardinality {
                min: 0,
                max: input.cardinality().max,
            }
        };
        Self {
            laws: Laws {
                produced: input.produced().union(&SlotSet::new(output.slots())),
                output,
                free,
                cardinality,
                ordered: false,
            },
            node: RelationNode::Aggregate {
                input: Box::new(input),
                groups,
                terms,
            },
        }
    }

    pub fn order(input: Relation, terms: Vec<BoundOrderTerm>) -> Self {
        let mut laws = Laws::inherited(&input);
        laws.free = terms.iter().fold(laws.free, |free, term| {
            free.union(&term.expression.free_slots().without(input.produced()))
        });
        laws.ordered = true;
        Self {
            node: RelationNode::Order {
                input: Box::new(input),
                terms,
            },
            laws,
        }
    }

    pub fn slice(input: Relation, offset: usize, limit: Option<usize>) -> Self {
        let mut laws = Laws::inherited(&input);
        laws.cardinality.min = 0;
        if let Some(limit) = limit.and_then(|limit| i64::try_from(limit).ok())
            && (laws.cardinality.max == UNBOUNDED || limit <= laws.cardinality.max)
        {
            laws.cardinality.max = limit;
        }
        Self {
            node: RelationNode::Slice {
                input: Box::new(input),
                offset,
                limit,
            },
            laws,
        }
    }

    pub fn reference(
        binding: impl Into<String>,
        scope: impl Into<String>,
        fields: Vec<Field>,
        canonical: Vec<SlotId>,
    ) -> Self {
        let laws = Laws::leaf(RowType { fields }, Cardinality::MANY);
        Self {
            node: RelationNode::Ref {
                binding: binding.into(),
                scope: scope.into(),
                canonical,
            },
            laws,
        }
    }

    pub fn recursive_reference(
        binding: impl Into<String>,
        scope: impl Into<String>,
        fields: Vec<Field>,
        canonical: Vec<SlotId>,
    ) -> Self {
        let laws = Laws::leaf(RowType { fields }, Cardinality::MANY);
        Self {
            node: RelationNode::RecursiveRef {
                binding: binding.into(),
                scope: scope.into(),
                canonical,
            },
            laws,
        }
    }

    pub fn distinct(input: Relation) -> Self {
        let mut laws = Laws::inherited(&input);
        laws.cardinality.min = 0;
        laws.ordered = false;
        Self {
            node: RelationNode::Distinct(Box::new(input)),
            laws,
        }
    }

    /// Returns the catalog table retained by a scan relation.
    pub(crate) fn scan_table(&self) -> &Table {
        match &self.node {
            RelationNode::Scan { table, .. } => table,
            _ => unreachable!("scan table requested from a non-scan relation"),
        }
    }

    pub fn output(&self) -> &RowType {
        &self.laws.output
    }

    pub fn free_slots(&self) -> &SlotSet {
        &self.laws.free
    }

    pub fn produced(&self) -> &SlotSet {
        &self.laws.produced
    }

    pub fn cardinality(&self) -> Cardinality {
        self.laws.cardinality
    }

    pub fn is_ordered(&self) -> bool {
        self.laws.ordered
    }

    pub fn refine_cardinality(&mut self, refinement: Cardinality) {
        if refinement.min > self.laws.cardinality.min {
            self.laws.cardinality.min = refinement.min;
        }
        if refinement.max != UNBOUNDED
            && (self.laws.cardinality.max == UNBOUNDED
                || refinement.max < self.laws.cardinality.max)
        {
            self.laws.cardinality.max = refinement.max;
        }
    }

    pub fn inputs(&self) -> Vec<&Relation> {
        match &self.node {
            RelationNode::Scan { .. }
            | RelationNode::Rows { .. }
            | RelationNode::Ref { .. }
            | RelationNode::RecursiveRef { .. } => Vec::new(),
            RelationNode::Filter { input, .. }
            | RelationNode::Project { input, .. }
            | RelationNode::Aggregate { input, .. }
            | RelationNode::Order { input, .. }
            | RelationNode::Slice { input, .. }
            | RelationNode::Distinct(input) => vec![input],
            RelationNode::Join { left, right, .. }
            | RelationNode::Intersect { left, right, .. }
            | RelationNode::Except { left, right, .. } => vec![left, right],
            RelationNode::Concatenate { inputs, .. } => inputs.iter().collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub name: String,
    pub root: Relation,
    pub output: RowType,
    pub plan_sensitive: bool,
    pub recursive: bool,
    pub step: Option<Relation>,
    pub accumulation: Option<RecursiveAccumulation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub root: Relation,
    pub cardinality: RootCardinality,
    pub bindings: Vec<Binding>,
    pub next_slot: SlotId,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum BoundError {
    #[error("planner: scalar crossing needs a single-column relation, got {0} columns")]
    ScalarArity(usize),
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType};

    use super::*;

    fn tasks() -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "tasks".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: [
                ("id", ScalarType::Text, false),
                ("board_id", ScalarType::Text, false),
                ("status", ScalarType::Text, false),
                ("priority", ScalarType::Int64, false),
                ("assignee_id", ScalarType::Text, true),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, scalar_type, nullable))| Column {
                id: format!("c{}", index + 1).into(),
                schema_id: SchemaId::new((index + 1) as u32).unwrap(),
                name: name.into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type,
                nullable,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            })
            .collect(),
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan() -> Relation {
        Relation::scan(tasks(), "t", (0..5).map(SlotId).collect())
    }

    fn slot(relation: &Relation, name: &str) -> Expr {
        let field = relation.output().lookup(name).unwrap();
        Expr::slot(field.slot, name, field.value_type.clone())
    }

    #[test]
    fn scan_filter_project_and_correlation_laws_match_the_contract() {
        let scan = scan();
        assert_eq!(scan.output().fields.len(), 5);
        assert!(scan.output().fields[4].value_type.nullable);
        assert!(scan.free_slots().is_empty());
        assert_eq!(scan.cardinality(), Cardinality::MANY);

        let outer = Expr::slot(100, "b.id", Type::scalar(Kind::Text, false));
        let predicate = Expr::binary(BinaryOp::Eq, slot(&scan, "board_id"), outer);
        let mut filter = Relation::filter(scan.clone(), predicate);
        assert_eq!(filter.free_slots().slots(), [SlotId(100)]);
        filter.refine_cardinality(Cardinality { min: 0, max: 1 });
        filter.refine_cardinality(Cardinality { min: 0, max: 100 });
        assert!(filter.cardinality().at_most_one());

        let project = Relation::project(
            scan.clone(),
            "shaped",
            vec![ProjectField {
                name: "score".into(),
                slot: SlotId(10),
                expression: Expr::binary(
                    BinaryOp::Mul,
                    slot(&scan, "priority"),
                    Expr::literal(Value::Int64(2)),
                ),
            }],
        );
        assert_eq!(project.output().fields[0].value_type.kind, Kind::Int64);
        assert!(project.produced().contains(SlotId(10)));
        assert!(project.produced().contains(SlotId(3)));
    }

    #[test]
    fn aggregate_join_slice_crossing_and_slotset_laws_match_the_contract() {
        let scan = scan();
        let global = Relation::aggregate(
            scan.clone(),
            Vec::new(),
            vec![BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "n".into(),
                slot: SlotId(20),
                value_type: aggregate_term_type(AggregateFunction::Count, None),
            }],
        );
        assert_eq!(global.cardinality(), Cardinality { min: 1, max: 1 });
        assert_eq!(
            global.output().fields[0].value_type,
            Type::scalar(Kind::Int64, false)
        );
        assert_eq!(
            Expr::scalar(global).unwrap().value_type(),
            Type::scalar(Kind::Int64, true)
        );
        assert_eq!(
            Expr::scalar(scan.clone()).unwrap_err(),
            BoundError::ScalarArity(5)
        );

        let ordered = Relation::order(
            scan.clone(),
            vec![BoundOrderTerm {
                expression: slot(&scan, "priority"),
                descending: true,
            }],
        );
        assert!(Relation::slice(ordered, 0, Some(5)).is_ordered());
        assert_eq!(Relation::slice(scan, 0, Some(1)).cardinality().max, 1);

        let slots = SlotSet::new([SlotId(0), SlotId(63), SlotId(64), SlotId(130)]);
        assert!(slots.contains(SlotId(130)));
        assert!(
            !slots
                .without(&SlotSet::new([SlotId(64)]))
                .contains(SlotId(64))
        );
    }

    #[test]
    fn oversized_slice_limit_does_not_wrap_cardinality() {
        let expected = i64::try_from(usize::MAX).unwrap_or(UNBOUNDED);
        assert_eq!(
            Relation::slice(scan(), 0, Some(usize::MAX))
                .cardinality()
                .max,
            expected
        );
    }

    #[test]
    fn join_grouping_and_set_operation_laws_match_the_contract() {
        let left = scan();
        let mut users = tasks();
        users.id = "t2".into();
        users.name = "users".into();
        users.columns.truncate(1);
        let right = Relation::scan(users, "u", vec![SlotId(50)]);
        let on = Expr::binary(
            BinaryOp::Eq,
            slot(&left, "assignee_id"),
            Expr::slot(SlotId(50), "u.id", Type::scalar(Kind::Text, false)),
        );
        let inner = Relation::join(left.clone(), right.clone(), JoinKind::Inner, on.clone());
        assert_eq!(inner.output().fields.len(), 6);
        assert!(!inner.output().fields[5].value_type.nullable);
        assert!(inner.free_slots().is_empty());
        let outer = Relation::join(left.clone(), right, JoinKind::Left, on);
        assert!(outer.output().fields[5].value_type.nullable);

        let grouped = Relation::aggregate(
            left.clone(),
            vec![BoundGroupTerm {
                name: "status".into(),
                slot: SlotId(30),
                expression: slot(&left, "status"),
            }],
            vec![BoundAggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "n".into(),
                slot: SlotId(31),
                value_type: aggregate_term_type(AggregateFunction::Count, None),
            }],
        );
        assert_eq!(grouped.cardinality(), Cardinality::MANY);
        assert!(grouped.produced().contains(SlotId(30)));
        assert!(grouped.produced().contains(SlotId(31)));

        let fields = vec![Field {
            name: "x".into(),
            slot: SlotId(60),
            value_type: Type::scalar(Kind::Int64, false),
        }];
        let one = Relation::rows(
            "one",
            vec![Field {
                slot: SlotId(61),
                ..fields[0].clone()
            }],
            vec![vec![Value::Int64(1)]],
        );
        let two = Relation::rows(
            "two",
            vec![Field {
                slot: SlotId(62),
                ..fields[0].clone()
            }],
            vec![vec![Value::Int64(1)], vec![Value::Int64(2)]],
        );
        let concatenate =
            Relation::concatenate(vec![one.clone(), two.clone()], "all", fields.clone());
        assert_eq!(concatenate.cardinality(), Cardinality { min: 3, max: 3 });
        assert!(concatenate.produced().contains(SlotId(60)));
        let intersect = Relation::intersect(
            one.clone(),
            two.clone(),
            SetQuantifier::All,
            "common",
            fields.clone(),
        );
        assert_eq!(intersect.cardinality(), Cardinality { min: 0, max: 1 });
        let except = Relation::except(two, one, SetQuantifier::Distinct, "remaining", fields);
        assert_eq!(except.cardinality(), Cardinality { min: 0, max: 2 });
        assert!(!Relation::distinct(concatenate).is_ordered());
    }
}
