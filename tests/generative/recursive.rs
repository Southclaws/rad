use std::collections::{BTreeMap, HashMap, HashSet};

use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, IndexDef, ScalarType, Schema, TableDef};
use rad::engine::lir::{
    BinaryOp, Expr, JoinKind, Kind, Literal, OrderTerm, ProjectField, Query, RawScalar,
    RecursiveAccumulation, Relation, RootCardinality, RowsColumn, UnaryOp, Value,
};

use super::{Case, CaseKind, Choices, decisions_from_seed};

pub fn recursive_case(seed: u64) -> Case {
    generate(decisions_from_seed(seed))
}

pub fn generate(decisions: Vec<u64>) -> Case {
    let mut choices = Choices::new(&decisions);
    let catalog = graph_catalog();
    let node_count = choices.range(2, 6);
    let nodes = (0..node_count)
        .map(|index| format!("n{index}"))
        .collect::<Vec<_>>();
    let acyclic = choices.coin();
    let edge_attempts = choices.range(0, node_count * 2);
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for _ in 0..edge_attempts {
        let mut source = choices.index(node_count);
        let mut destination = choices.index(node_count);
        if acyclic {
            if source == destination {
                continue;
            }
            if source > destination {
                std::mem::swap(&mut source, &mut destination);
            }
        }
        if !seen.insert((source, destination)) {
            continue;
        }
        rows.push(HashMap::from([
            ("src".into(), Value::Text(nodes[source].clone())),
            ("dst".into(), Value::Text(nodes[destination].clone())),
            ("w".into(), Value::Int64(choices.range(0, 5) as i64)),
            (
                "note".into(),
                if choices.chance(3) {
                    Value::Null(ScalarType::Text)
                } else {
                    Value::Text(["", "x", "y"][choices.index(3)].into())
                },
            ),
        ]));
    }
    let data = BTreeMap::from([("edges".into(), rows)]);
    let query = recursive_query(&nodes, acyclic, &mut choices);
    Case {
        kind: CaseKind::Recursive,
        decisions,
        catalog,
        data,
        query,
        ordered: false,
    }
}

fn graph_catalog() -> Schema {
    Schema::from_definitions(vec![TableDef {
        id: id(1),
        name: "edges".into(),
        columns: vec![
            column(1, "src", ScalarType::Text, false),
            column(2, "dst", ScalarType::Text, false),
            column(3, "w", ScalarType::Int64, false),
            column(4, "note", ScalarType::Text, true),
        ],
        primary_key: vec!["src".into(), "dst".into()],
        indexes: vec![IndexDef {
            name: "edges_src_idx".into(),
            columns: vec!["src".into()],
            unique: false,
        }],
        foreign_keys: Vec::new(),
    }])
}

fn recursive_query(nodes: &[String], acyclic: bool, choices: &mut Choices<'_>) -> Query {
    let root_count = choices.range(1, nodes.len().min(3));
    let mut roots = nodes.to_vec();
    for index in 0..roots.len() {
        let other = index + choices.index(roots.len() - index);
        roots.swap(index, other);
    }
    roots.truncate(root_count);

    let shape = if acyclic { choices.index(7) } else { 0 };
    let accumulation = if acyclic && choices.coin() {
        RecursiveAccumulation::All
    } else {
        RecursiveAccumulation::New
    };
    let states = state_shape(shape);
    let mut columns = vec![RowsColumn {
        name: "id".into(),
        kind: Kind::Text,
        nullable: false,
    }];
    columns.extend(states.iter().map(|state| RowsColumn {
        name: state.name.into(),
        kind: state.kind,
        nullable: state.nullable,
    }));
    let values = roots
        .into_iter()
        .map(|root| {
            let mut row = vec![RawScalar::Text(root)];
            row.extend(states.iter().map(|state| state.initial.clone()));
            row
        })
        .collect();
    let anchor = Relation::Rows {
        scope: "a".into(),
        columns,
        values,
    };

    let mut fields = vec![ProjectField {
        name: "id".into(),
        expression: column_expr("e", "dst"),
    }];
    fields.extend(states.iter().map(|state| ProjectField {
        name: state.name.into(),
        expression: (state.step)(),
    }));
    let step = Relation::Project {
        input: Box::new(Relation::Join {
            left: Box::new(Relation::Scan {
                table: "edges".into(),
                scope: "e".into(),
            }),
            right: Box::new(Relation::RecursiveRef {
                binding: "rec".into(),
                scope: "p".into(),
            }),
            kind: JoinKind::Inner,
            on: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(column_expr("e", "src")),
                right: Box::new(column_expr("p", "id")),
            },
        }),
        scope: Some("s".into()),
        spread: Vec::new(),
        fields,
    };
    Query {
        root: Relation::Order {
            input: Box::new(Relation::Ref {
                binding: "rec".into(),
                scope: "r".into(),
            }),
            terms: vec![OrderTerm {
                expression: literal_bool(true),
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::from([(
            "rec".into(),
            Relation::Recursive {
                anchor: Box::new(anchor),
                step: Box::new(step),
                accumulation,
            },
        )]),
    }
}

struct State {
    name: &'static str,
    kind: Kind,
    nullable: bool,
    initial: RawScalar,
    step: fn() -> Expr,
}

fn state_shape(shape: usize) -> Vec<State> {
    let mut states = Vec::new();
    for name in match shape {
        0 => &[][..],
        1 => &["depth"][..],
        2 => &["cost"][..],
        3 => &["tag"][..],
        4 => &["depth", "flag"][..],
        5 => &["tag", "num"][..],
        _ => &["depth", "cost", "tag", "flag", "tag2"][..],
    } {
        states.push(match *name {
            "depth" => State {
                name,
                kind: Kind::Int64,
                nullable: false,
                initial: RawScalar::Number("0".into()),
                step: depth,
            },
            "cost" => State {
                name,
                kind: Kind::Int64,
                nullable: false,
                initial: RawScalar::Number("0".into()),
                step: cost,
            },
            "tag" | "tag2" => State {
                name,
                kind: Kind::Text,
                nullable: true,
                initial: RawScalar::Null,
                step: note,
            },
            "flag" => State {
                name,
                kind: Kind::Bool,
                nullable: false,
                initial: RawScalar::Bool(true),
                step: flag,
            },
            "num" => State {
                name,
                kind: Kind::Int64,
                nullable: true,
                initial: RawScalar::Null,
                step: weight,
            },
            _ => unreachable!(),
        });
    }
    states
}

fn depth() -> Expr {
    add(column_expr("p", "depth"), literal_int(1))
}
fn cost() -> Expr {
    add(column_expr("p", "cost"), column_expr("e", "w"))
}
fn note() -> Expr {
    column_expr("e", "note")
}
fn weight() -> Expr {
    column_expr("e", "w")
}
fn flag() -> Expr {
    Expr::Unary {
        op: UnaryOp::Not,
        expression: Box::new(column_expr("p", "flag")),
    }
}
fn add(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op: BinaryOp::Add,
        left: Box::new(left),
        right: Box::new(right),
    }
}
fn column_expr(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}
fn literal_int(value: i64) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Number(value.to_string()),
        kind: Some(Kind::Int64),
    })
}
fn literal_bool(value: bool) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Bool(value),
        kind: Some(Kind::Bool),
    })
}
fn column(id_value: u32, name: &str, scalar_type: ScalarType, nullable: bool) -> ColumnDef {
    ColumnDef {
        id: id(id_value),
        name: name.into(),
        scalar_type,
        nullable,
        format: String::new(),
        default: None,
    }
}
fn id(value: u32) -> SchemaId {
    SchemaId::new(value).unwrap()
}
