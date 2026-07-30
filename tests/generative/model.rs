use std::collections::{BTreeMap, HashMap};

use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, IndexDef, ScalarType, Schema, TableDef};
use rad::engine::lir::{
    BinaryOp, Expr, Kind, Literal, OrderTerm, ProjectField, Query, RawScalar, Relation,
    RootCardinality, Row, Value,
};
use serde_json::json;

use super::{Case, CaseKind, Choices, TestResult, check_expected, decisions_from_seed};

#[derive(Clone, Debug)]
pub struct ModelCase {
    pub decisions: Vec<u64>,
    pub case: Case,
    expected: serde_json::Value,
}

#[derive(Clone)]
struct ModelRow {
    id: String,
    score: Option<i64>,
    active: bool,
    label: String,
}

impl ModelCase {
    pub fn from_seed(seed: u64) -> Self {
        Self::generate(decisions_from_seed(seed))
    }

    pub fn generate(decisions: Vec<u64>) -> Self {
        let mut choices = Choices::new(&decisions);
        let table = table();
        let rows = (0..choices.range(0, 8))
            .map(|index| ModelRow {
                id: format!("item-{index}"),
                score: (!choices.chance(4)).then(|| [-3, -2, -1, 0, 1, 2, 3][choices.index(7)]),
                active: choices.coin(),
                label: ["a", "b", "c"][choices.index(3)].into(),
            })
            .collect::<Vec<_>>();
        let threshold = [-2, -1, 0, 1, 2][choices.index(5)];
        let wanted_active = choices.coin();
        let delta = [-2, -1, 0, 1, 2][choices.index(5)];
        let descending = choices.coin();
        let offset = choices.range(0, 3);
        let limit = choices.coin().then(|| choices.range(0, 5));

        let mut expected = rows
            .iter()
            .filter(|row| row.active == wanted_active && row.score.is_some_and(|v| v >= threshold))
            .cloned()
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.id.cmp(&right.id));
        if descending {
            expected.reverse();
        }
        let expected = expected
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .map(|row| {
                json!({
                    "id": row.id,
                    "score": row.score.expect("NULL scores were filtered") + delta,
                    "label": row.label,
                })
            })
            .collect::<Vec<_>>();

        let data = BTreeMap::from([(
            table.name.clone(),
            rows.iter().map(stored_row).collect::<Vec<_>>(),
        )]);
        let query = query(threshold, wanted_active, delta, descending, offset, limit);
        Self {
            decisions: decisions.clone(),
            case: Case {
                kind: CaseKind::Relational,
                decisions,
                catalog: Schema::from_definitions(vec![table]),
                data,
                query,
                ordered: true,
            },
            expected: serde_json::Value::Array(expected),
        }
    }
}

pub async fn check_model(case: &ModelCase) -> TestResult<()> {
    check_expected(&case.case, &case.expected).await
}

fn table() -> TableDef {
    TableDef {
        id: id(1),
        name: "model_items".into(),
        columns: vec![
            column(1, "id", ScalarType::Text, false),
            column(2, "score", ScalarType::Int64, true),
            column(3, "active", ScalarType::Bool, false),
            column(4, "label", ScalarType::Text, false),
        ],
        primary_key: vec!["id".into()],
        indexes: vec![IndexDef {
            name: "model_items_score".into(),
            columns: vec!["score".into()],
            unique: false,
        }],
        foreign_keys: Vec::new(),
    }
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
    SchemaId::new(value).expect("model schema IDs are positive")
}

fn stored_row(row: &ModelRow) -> Row {
    Row::from([
        ("id".into(), Value::Text(row.id.clone())),
        (
            "score".into(),
            row.score
                .map(Value::Int64)
                .unwrap_or(Value::Null(ScalarType::Int64)),
        ),
        ("active".into(), Value::Bool(row.active)),
        ("label".into(), Value::Text(row.label.clone())),
    ])
}

fn query(
    threshold: i64,
    wanted_active: bool,
    delta: i64,
    descending: bool,
    offset: usize,
    limit: Option<usize>,
) -> Query {
    let stored = "stored";
    let projected = "projected";
    let predicate = binary(
        BinaryOp::And,
        binary(
            BinaryOp::Eq,
            column_expr(stored, "active"),
            literal_bool(wanted_active),
        ),
        binary(
            BinaryOp::Gte,
            column_expr(stored, "score"),
            literal_int(threshold),
        ),
    );
    let filtered = Relation::Filter {
        input: Box::new(Relation::Scan {
            table: "model_items".into(),
            scope: stored.into(),
        }),
        predicate,
    };
    let projected_relation = Relation::Project {
        input: Box::new(filtered),
        scope: Some(projected.into()),
        spread: Vec::new(),
        fields: vec![
            field("id", column_expr(stored, "id")),
            field(
                "score",
                binary(
                    BinaryOp::Add,
                    column_expr(stored, "score"),
                    literal_int(delta),
                ),
            ),
            field("label", column_expr(stored, "label")),
        ],
    };
    Query {
        root: Relation::Slice {
            input: Box::new(Relation::Order {
                input: Box::new(projected_relation),
                terms: vec![OrderTerm {
                    expression: column_expr(projected, "id"),
                    descending,
                }],
            }),
            offset,
            limit,
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn field(name: &str, expression: Expr) -> ProjectField {
    ProjectField {
        name: name.into(),
        expression,
    }
}

fn column_expr(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
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
