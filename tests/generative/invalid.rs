use std::collections::HashMap;

use rad::engine::exec::ErrorReason;
use rad::engine::lir::{
    BinaryOp, Expr, JoinKind, Kind, Literal, OrderTerm, ProjectField, Query, RawScalar, Relation,
    RootCardinality,
};

use super::Case;

#[derive(Clone, Debug)]
pub struct Variant {
    pub name: &'static str,
    pub query: Query,
    pub reason: ErrorReason,
}

/// Produce one-edit-near-valid queries from a generated catalog. Each variant
/// isolates one stable rejection reason while retaining valid table, column,
/// type, and ordering context everywhere else.
pub fn variants(case: &Case) -> Vec<Variant> {
    let table = case
        .catalog
        .tables
        .first()
        .expect("generated catalogs contain a table");
    let table_name = table.name.clone();
    let primary_key = table
        .primary_key
        .first()
        .expect("generated tables contain a primary key")
        .clone();
    let scope = "valid";

    vec![
        Variant {
            name: "unknown table",
            query: query(ordered(scan("__missing_table__", scope))),
            reason: ErrorReason::UnknownTable,
        },
        Variant {
            name: "unknown column",
            query: query(ordered(Relation::Filter {
                input: Box::new(scan(&table_name, scope)),
                predicate: equality(column(scope, "__missing_column__"), text("value")),
            })),
            reason: ErrorReason::UnknownColumn,
        },
        Variant {
            name: "unknown scope",
            query: query(ordered(Relation::Filter {
                input: Box::new(scan(&table_name, scope)),
                predicate: equality(column("__missing_scope__", &primary_key), text("value")),
            })),
            reason: ErrorReason::UnknownScope,
        },
        Variant {
            name: "unknown binding",
            query: query(ordered(Relation::Ref {
                binding: "__missing_binding__".into(),
                scope: "reference".into(),
            })),
            reason: ErrorReason::UnknownBinding,
        },
        Variant {
            name: "duplicate scope",
            query: query(ordered(Relation::Join {
                left: Box::new(scan(&table_name, "duplicate")),
                right: Box::new(scan(&table_name, "duplicate")),
                kind: JoinKind::Inner,
                on: boolean(true),
            })),
            reason: ErrorReason::DuplicateScope,
        },
        Variant {
            name: "non-boolean predicate",
            query: query(ordered(Relation::Filter {
                input: Box::new(scan(&table_name, scope)),
                predicate: integer(1),
            })),
            reason: ErrorReason::TypeMismatch,
        },
        Variant {
            name: "projection collision",
            query: query(ordered(Relation::Project {
                input: Box::new(scan(&table_name, scope)),
                scope: Some("projected".into()),
                spread: vec![scope.into()],
                fields: vec![ProjectField {
                    name: primary_key.clone(),
                    expression: column(scope, &primary_key),
                }],
            })),
            reason: ErrorReason::ProjectionCollision,
        },
        Variant {
            name: "nondeterministic collection",
            query: Query {
                root: scan(&table_name, scope),
                cardinality: RootCardinality::Many,
                bindings: HashMap::new(),
            },
            reason: ErrorReason::NondeterministicOrder,
        },
        Variant {
            name: "scalar arity",
            query: Query {
                root: ordered(scan(&table_name, scope)),
                cardinality: RootCardinality::Scalar,
                bindings: HashMap::new(),
            },
            reason: ErrorReason::ScalarArity,
        },
    ]
}

fn query(root: Relation) -> Query {
    Query {
        root,
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn scan(table: &str, scope: &str) -> Relation {
    Relation::Scan {
        table: table.into(),
        scope: scope.into(),
    }
}

fn ordered(input: Relation) -> Relation {
    Relation::Order {
        input: Box::new(input),
        terms: vec![OrderTerm {
            expression: boolean(true),
            descending: false,
        }],
    }
}

fn equality(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op: BinaryOp::Eq,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn column(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}

fn text(value: &str) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Text(value.into()),
        kind: Some(Kind::Text),
    })
}

fn integer(value: i64) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Number(value.to_string()),
        kind: Some(Kind::Int64),
    })
}

fn boolean(value: bool) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Bool(value),
        kind: Some(Kind::Bool),
    })
}
