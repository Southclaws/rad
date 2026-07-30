use std::collections::{BTreeMap, HashMap};

use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, ScalarType, Schema, TableDef};
use rad::engine::lir::{
    BinaryOp, Expr, Kind, Literal, OrderTerm, ProjectField, Query, RawScalar, Relation,
    RootCardinality, Row, Value,
};

use super::{Case, CaseKind, Choices, decisions_from_seed};

pub fn nested_identity_case(seed: u64) -> Case {
    generate(decisions_from_seed(seed))
}

pub fn generate(decisions: Vec<u64>) -> Case {
    let mut choices = Choices::new(&decisions);
    let mode = choices.index(3);
    let second_value = if mode == 1 { 1.0 } else { -0.0 };
    let groups = table(
        1,
        "groups",
        vec![column_def(2, "id", ScalarType::Text)],
        vec!["id".into()],
    );
    let items = table(
        3,
        "items",
        vec![
            column_def(4, "id", ScalarType::Text),
            column_def(5, "group_id", ScalarType::Text),
            column_def(6, "value", ScalarType::Float64),
        ],
        vec!["id".into()],
    );
    let data = BTreeMap::from([
        (
            "groups".into(),
            vec![text_row(&[("id", "a")]), text_row(&[("id", "b")])],
        ),
        (
            "items".into(),
            vec![
                item_row("a-item", "a", 0.0),
                item_row("b-item", "b", second_value),
            ],
        ),
    ]);

    Case {
        kind: CaseKind::NestedIdentity,
        decisions,
        catalog: Schema::from_definitions(vec![groups, items]),
        data,
        query: if mode == 0 {
            scalar_query()
        } else {
            nested_query()
        },
        ordered: true,
    }
}

fn scalar_query() -> Query {
    let projected = Relation::Project {
        input: Box::new(Relation::Scan {
            table: "items".into(),
            scope: "item".into(),
        }),
        scope: Some("projected".into()),
        spread: Vec::new(),
        fields: vec![ProjectField {
            name: "value".into(),
            expression: column("item", "value"),
        }],
    };
    Query {
        root: Relation::Order {
            input: Box::new(Relation::Distinct(Box::new(projected))),
            terms: vec![OrderTerm {
                expression: column("projected", "value"),
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn nested_query() -> Query {
    let item_scan = Relation::Scan {
        table: "items".into(),
        scope: "item".into(),
    };
    let correlated = Relation::Filter {
        input: Box::new(item_scan),
        predicate: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(column("item", "group_id")),
            right: Box::new(column("group", "id")),
        },
    };
    let ordered_items = Relation::Order {
        input: Box::new(correlated),
        terms: vec![OrderTerm {
            expression: column("item", "id"),
            descending: false,
        }],
    };
    let projected = Relation::Project {
        input: Box::new(Relation::Scan {
            table: "groups".into(),
            scope: "group".into(),
        }),
        scope: Some("projected".into()),
        spread: Vec::new(),
        fields: vec![ProjectField {
            name: "items".into(),
            expression: Expr::Array(Box::new(ordered_items)),
        }],
    };
    Query {
        root: Relation::Order {
            input: Box::new(Relation::Distinct(Box::new(projected))),
            terms: vec![OrderTerm {
                expression: Expr::Literal(Literal {
                    raw: RawScalar::Bool(true),
                    kind: Some(Kind::Bool),
                }),
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn table(id: u32, name: &str, columns: Vec<ColumnDef>, primary_key: Vec<String>) -> TableDef {
    TableDef {
        id: SchemaId::new(id).unwrap(),
        name: name.into(),
        columns,
        primary_key,
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

fn column_def(id: u32, name: &str, scalar_type: ScalarType) -> ColumnDef {
    ColumnDef {
        id: SchemaId::new(id).unwrap(),
        name: name.into(),
        scalar_type,
        nullable: false,
        format: String::new(),
        default: None,
    }
}

fn text_row(values: &[(&str, &str)]) -> Row {
    values
        .iter()
        .map(|(name, value)| ((*name).into(), Value::Text((*value).into())))
        .collect()
}

fn item_row(id: &str, group: &str, value: f64) -> Row {
    Row::from([
        ("id".into(), Value::Text(id.into())),
        ("group_id".into(), Value::Text(group.into())),
        ("value".into(), Value::Float64(value)),
    ])
}

fn column(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_family_covers_scalar_signed_zero_and_both_nested_cases() {
        let cases = (0..64).map(nested_identity_case).collect::<Vec<_>>();
        for mode in 0..3 {
            assert!(
                cases.iter().any(|case| case.decisions[0] % 3 == mode),
                "seed family did not reach mode {mode}"
            );
        }
        assert!(cases.iter().any(|case| {
            matches!(
                &case.data["items"][1]["value"],
                Value::Float64(value) if *value == 1.0
            )
        }));
        assert!(cases.iter().any(|case| {
            matches!(
                &case.data["items"][1]["value"],
                Value::Float64(value) if value.to_bits() == (-0.0_f64).to_bits()
            )
        }));
    }
}
