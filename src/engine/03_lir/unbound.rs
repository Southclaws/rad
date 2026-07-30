use std::collections::HashMap;

use super::Kind;

/// A raw scalar preserves number text until binding supplies its target type.
#[derive(Clone, Debug, PartialEq)]
pub enum RawScalar {
    Null,
    Text(String),
    Number(String),
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Literal {
    pub raw: RawScalar,
    pub kind: Option<Kind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Not,
    Negate,
    IsNull,
    IsNotNull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BranchArm {
    pub when: Expr,
    pub then: Expr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextMatchPart {
    Literal(String),
    AnyMany,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextComparison {
    #[default]
    Exact,
    UnicodeSimpleFold,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column {
        scope: String,
        name: String,
    },
    Unary {
        op: UnaryOp,
        expression: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Cast {
        expression: Box<Expr>,
        to: Kind,
    },
    Branch {
        arms: Vec<BranchArm>,
        otherwise: Box<Expr>,
    },
    TextMatch {
        value: Box<Expr>,
        parts: Vec<TextMatchPart>,
        comparison: TextComparison,
    },
    Exists(Box<Relation>),
    First(Box<Relation>),
    Scalar(Box<Relation>),
    Array(Box<Relation>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowsColumn {
    pub name: String,
    pub kind: Kind,
    pub nullable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectField {
    pub name: String,
    pub expression: Expr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetQuantifier {
    All,
    Distinct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateFunction {
    Count,
    Sum,
    Average,
    Min,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggregateTerm {
    pub function: AggregateFunction,
    pub argument: Option<Expr>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupTerm {
    pub name: String,
    pub expression: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderTerm {
    pub expression: Expr,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecursiveAccumulation {
    All,
    New,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Relation {
    Scan {
        table: String,
        scope: String,
    },
    Rows {
        scope: String,
        columns: Vec<RowsColumn>,
        values: Vec<Vec<RawScalar>>,
    },
    Filter {
        input: Box<Relation>,
        predicate: Expr,
    },
    Project {
        input: Box<Relation>,
        scope: Option<String>,
        spread: Vec<String>,
        fields: Vec<ProjectField>,
    },
    Join {
        left: Box<Relation>,
        right: Box<Relation>,
        kind: JoinKind,
        on: Expr,
    },
    Concatenate {
        scope: String,
        inputs: Vec<Relation>,
    },
    Intersect {
        scope: String,
        left: Box<Relation>,
        right: Box<Relation>,
        quantifier: SetQuantifier,
    },
    Except {
        scope: String,
        left: Box<Relation>,
        right: Box<Relation>,
        quantifier: SetQuantifier,
    },
    Aggregate {
        input: Box<Relation>,
        scope: Option<String>,
        groups: Vec<GroupTerm>,
        terms: Vec<AggregateTerm>,
    },
    Order {
        input: Box<Relation>,
        terms: Vec<OrderTerm>,
    },
    Slice {
        input: Box<Relation>,
        offset: usize,
        limit: Option<usize>,
    },
    Ref {
        binding: String,
        scope: String,
    },
    RecursiveRef {
        binding: String,
        scope: String,
    },
    Recursive {
        anchor: Box<Relation>,
        step: Box<Relation>,
        accumulation: RecursiveAccumulation,
    },
    Distinct(Box<Relation>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootCardinality {
    Many,
    First,
    ExactlyOne,
    Scalar,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub root: Relation,
    pub cardinality: RootCardinality,
    pub bindings: HashMap<String, Relation>,
}
