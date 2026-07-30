use std::collections::{BTreeMap, HashMap, HashSet};

use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, ScalarType, Schema, TableDef};
use rad::engine::lir::{
    AggregateFunction, AggregateTerm, BinaryOp, Expr, GroupTerm, JoinKind, Kind, Literal,
    OrderTerm, ProjectField, Query, RawScalar, RecursiveAccumulation, Relation, RootCardinality,
    Row, RowsColumn, SetQuantifier, Value,
};
use serde_json::{Value as JsonValue, json};

use super::{Case, CaseKind, Choices, TestResult, check_expected, decisions_from_seed};

#[derive(Clone, Debug)]
pub struct SemanticModelCase {
    pub decisions: Vec<u64>,
    pub case: Case,
    expected: JsonValue,
}

impl SemanticModelCase {
    pub fn from_seed(seed: u64) -> Self {
        Self::generate(decisions_from_seed(seed))
    }

    pub fn generate(decisions: Vec<u64>) -> Self {
        let mut choices = Choices::new(&decisions);
        let (catalog, data, query, expected) = match choices.index(5) {
            0 => set_case(&mut choices),
            1 => aggregate_case(&mut choices),
            2 => cardinality_case(&mut choices),
            3 => crossing_case(&mut choices),
            _ => recursive_case(&mut choices),
        };
        Self {
            decisions: decisions.clone(),
            case: Case {
                kind: CaseKind::Relational,
                decisions,
                catalog,
                data,
                query,
                ordered: true,
            },
            expected,
        }
    }
}

pub async fn check_semantic_model(case: &SemanticModelCase) -> TestResult<()> {
    check_expected(&case.case, &case.expected).await
}

fn set_case(choices: &mut Choices<'_>) -> (Schema, BTreeMap<String, Vec<Row>>, Query, JsonValue) {
    let left = generated_nullable_ints(choices);
    let right = generated_nullable_ints(choices);
    let left_relation = int_rows("l", &left);
    let right_relation = int_rows("r", &right);
    let mode = choices.index(6);
    let (relation, mut expected) = match mode {
        0 => {
            let mut expected = left.clone();
            expected.extend(right.clone());
            (
                Relation::Concatenate {
                    scope: "s".into(),
                    inputs: vec![left_relation, right_relation],
                },
                expected,
            )
        }
        1 => {
            let mut expected = left.clone();
            expected.extend(right.clone());
            deduplicate(&mut expected);
            (
                Relation::Distinct(Box::new(Relation::Concatenate {
                    scope: "s".into(),
                    inputs: vec![left_relation, right_relation],
                })),
                expected,
            )
        }
        2 | 3 => {
            let distinct = mode == 3;
            (
                Relation::Intersect {
                    scope: "s".into(),
                    left: Box::new(left_relation),
                    right: Box::new(right_relation),
                    quantifier: if distinct {
                        SetQuantifier::Distinct
                    } else {
                        SetQuantifier::All
                    },
                },
                intersect(&left, &right, distinct),
            )
        }
        _ => {
            let distinct = mode == 5;
            (
                Relation::Except {
                    scope: "s".into(),
                    left: Box::new(left_relation),
                    right: Box::new(right_relation),
                    quantifier: if distinct {
                        SetQuantifier::Distinct
                    } else {
                        SetQuantifier::All
                    },
                },
                except(&left, &right, distinct),
            )
        }
    };
    expected.sort();
    let query = ordered_query(relation, "s", "v", RootCardinality::Many);
    let expected = JsonValue::Array(
        expected
            .into_iter()
            .map(|value| json!({"v": value}))
            .collect(),
    );
    (empty_schema(), BTreeMap::new(), query, expected)
}

fn aggregate_case(
    choices: &mut Choices<'_>,
) -> (Schema, BTreeMap<String, Vec<Row>>, Query, JsonValue) {
    let grouped = choices.coin();
    let values = (0..choices.range(0, 8))
        .map(|_| {
            (
                choices.coin(),
                if choices.chance(4) {
                    None
                } else {
                    Some([-2, -1, 0, 1, 2][choices.index(5)])
                },
            )
        })
        .collect::<Vec<_>>();
    let input = Relation::Rows {
        scope: "r".into(),
        columns: vec![
            RowsColumn {
                name: "g".into(),
                kind: Kind::Bool,
                nullable: false,
            },
            RowsColumn {
                name: "v".into(),
                kind: Kind::Int64,
                nullable: true,
            },
        ],
        values: values
            .iter()
            .map(|(group, value)| {
                vec![
                    RawScalar::Bool(*group),
                    value
                        .map(|value| RawScalar::Number(value.to_string()))
                        .unwrap_or(RawScalar::Null),
                ]
            })
            .collect(),
    };
    let groups = if grouped {
        vec![GroupTerm {
            name: "g".into(),
            expression: column("r", "g"),
        }]
    } else {
        Vec::new()
    };
    let relation = Relation::Aggregate {
        input: Box::new(input),
        scope: Some("a".into()),
        groups,
        terms: vec![
            AggregateTerm {
                function: AggregateFunction::Count,
                argument: None,
                name: "rows".into(),
            },
            AggregateTerm {
                function: AggregateFunction::Count,
                argument: Some(column("r", "v")),
                name: "values".into(),
            },
            aggregate(AggregateFunction::Sum, "sum"),
            aggregate(AggregateFunction::Average, "avg"),
            aggregate(AggregateFunction::Min, "min"),
            aggregate(AggregateFunction::Max, "max"),
        ],
    };
    let terms = if grouped {
        vec![OrderTerm {
            expression: column("a", "g"),
            descending: false,
        }]
    } else {
        vec![OrderTerm {
            expression: literal_bool(true),
            descending: false,
        }]
    };
    let query = Query {
        root: Relation::Order {
            input: Box::new(relation),
            terms,
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    };

    let mut partitions = BTreeMap::<Option<bool>, Vec<Option<i64>>>::new();
    if grouped {
        for (group, value) in values {
            partitions.entry(Some(group)).or_default().push(value);
        }
    } else {
        partitions.insert(None, values.into_iter().map(|(_, value)| value).collect());
    }
    let expected = JsonValue::Array(
        partitions
            .into_iter()
            .map(|(group, values)| aggregate_json(group, &values))
            .collect(),
    );
    (empty_schema(), BTreeMap::new(), query, expected)
}

fn cardinality_case(
    choices: &mut Choices<'_>,
) -> (Schema, BTreeMap<String, Vec<Row>>, Query, JsonValue) {
    let cardinality = [
        RootCardinality::Many,
        RootCardinality::First,
        RootCardinality::ExactlyOne,
        RootCardinality::Scalar,
    ][choices.index(4)];
    let count = match cardinality {
        RootCardinality::ExactlyOne => 1,
        RootCardinality::Scalar => choices.range(0, 1),
        RootCardinality::Many | RootCardinality::First => choices.range(0, 3),
    };
    let values = (0..count).map(|index| index as i64).collect::<Vec<_>>();
    let query = ordered_query(
        int_rows("r", &values.iter().copied().map(Some).collect::<Vec<_>>()),
        "r",
        "v",
        cardinality,
    );
    let expected = match cardinality {
        RootCardinality::Many => {
            JsonValue::Array(values.iter().map(|value| json!({"v": value})).collect())
        }
        RootCardinality::First => values
            .first()
            .map(|value| json!({"v": value}))
            .unwrap_or(JsonValue::Null),
        RootCardinality::ExactlyOne => json!({"v": values[0]}),
        RootCardinality::Scalar => values
            .first()
            .map(|value| json!(value))
            .unwrap_or(JsonValue::Null),
    };
    (empty_schema(), BTreeMap::new(), query, expected)
}

fn crossing_case(
    choices: &mut Choices<'_>,
) -> (Schema, BTreeMap<String, Vec<Row>>, Query, JsonValue) {
    let values = (0..choices.range(0, 3))
        .map(|index| Some(index as i64))
        .collect::<Vec<_>>();
    let scalar_values = values.iter().take(1).copied().collect::<Vec<_>>();
    let ordered = |scope: &str, values: &[Option<i64>]| Relation::Order {
        input: Box::new(int_rows(scope, values)),
        terms: vec![OrderTerm {
            expression: column(scope, "v"),
            descending: false,
        }],
    };
    let relation = Relation::Project {
        input: Box::new(Relation::Rows {
            scope: "o".into(),
            columns: vec![RowsColumn {
                name: "id".into(),
                kind: Kind::Text,
                nullable: false,
            }],
            values: vec![vec![RawScalar::Text("outer".into())]],
        }),
        scope: Some("p".into()),
        spread: Vec::new(),
        fields: vec![
            ProjectField {
                name: "exists".into(),
                expression: Expr::Exists(Box::new(int_rows("e", &values))),
            },
            ProjectField {
                name: "first".into(),
                expression: Expr::First(Box::new(ordered("f", &values))),
            },
            ProjectField {
                name: "scalar".into(),
                expression: Expr::Scalar(Box::new(int_rows("s", &scalar_values))),
            },
            ProjectField {
                name: "array".into(),
                expression: Expr::Array(Box::new(ordered("a", &values))),
            },
        ],
    };
    let query = Query {
        root: relation,
        cardinality: RootCardinality::ExactlyOne,
        bindings: HashMap::new(),
    };
    let objects = values
        .iter()
        .map(|value| json!({"v": value}))
        .collect::<Vec<_>>();
    let expected = json!({
        "exists": !values.is_empty(),
        "first": objects.first().cloned().unwrap_or(JsonValue::Null),
        "scalar": scalar_values.first().copied().flatten(),
        "array": objects,
    });
    (empty_schema(), BTreeMap::new(), query, expected)
}

fn recursive_case(
    choices: &mut Choices<'_>,
) -> (Schema, BTreeMap<String, Vec<Row>>, Query, JsonValue) {
    let accumulation = if choices.coin() {
        RecursiveAccumulation::New
    } else {
        RecursiveAccumulation::All
    };
    let node_count = choices.range(2, 5);
    let nodes = (0..node_count)
        .map(|index| format!("n{index}"))
        .collect::<Vec<_>>();
    let mut edge_set = HashSet::new();
    let mut edges = Vec::new();
    for _ in 0..choices.range(0, node_count * 2) {
        let mut source = choices.index(node_count);
        let mut destination = choices.index(node_count);
        if accumulation == RecursiveAccumulation::All {
            if source == destination {
                continue;
            }
            if source > destination {
                std::mem::swap(&mut source, &mut destination);
            }
        }
        if edge_set.insert((source, destination)) {
            edges.push((nodes[source].clone(), nodes[destination].clone()));
        }
    }
    let root = nodes[choices.index(nodes.len())].clone();
    let data = BTreeMap::from([(
        "edges".into(),
        edges
            .iter()
            .map(|(source, destination)| {
                Row::from([
                    ("src".into(), Value::Text(source.clone())),
                    ("dst".into(), Value::Text(destination.clone())),
                ])
            })
            .collect(),
    )]);
    let anchor = Relation::Rows {
        scope: "anchor".into(),
        columns: vec![RowsColumn {
            name: "id".into(),
            kind: Kind::Text,
            nullable: false,
        }],
        values: vec![vec![RawScalar::Text(root.clone())]],
    };
    let step = Relation::Project {
        input: Box::new(Relation::Join {
            left: Box::new(Relation::Scan {
                table: "edges".into(),
                scope: "edge".into(),
            }),
            right: Box::new(Relation::RecursiveRef {
                binding: "reach".into(),
                scope: "previous".into(),
            }),
            kind: JoinKind::Inner,
            on: binary(
                BinaryOp::Eq,
                column("edge", "src"),
                column("previous", "id"),
            ),
        }),
        scope: Some("next".into()),
        spread: Vec::new(),
        fields: vec![ProjectField {
            name: "id".into(),
            expression: column("edge", "dst"),
        }],
    };
    let query = Query {
        root: Relation::Order {
            input: Box::new(Relation::Ref {
                binding: "reach".into(),
                scope: "result".into(),
            }),
            terms: vec![OrderTerm {
                expression: column("result", "id"),
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::from([(
            "reach".into(),
            Relation::Recursive {
                anchor: Box::new(anchor),
                step: Box::new(step),
                accumulation,
            },
        )]),
    };
    let mut result = vec![root.clone()];
    let mut frontier = vec![root];
    let mut seen = result.iter().cloned().collect::<HashSet<_>>();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for current in frontier {
            for (_, destination) in edges.iter().filter(|(source, _)| *source == current) {
                if accumulation == RecursiveAccumulation::All || seen.insert(destination.clone()) {
                    result.push(destination.clone());
                    next.push(destination.clone());
                }
            }
        }
        frontier = next;
    }
    result.sort();
    let expected = JsonValue::Array(result.into_iter().map(|id| json!({"id": id})).collect());
    (
        Schema::from_definitions(vec![TableDef {
            id: schema_id(1),
            name: "edges".into(),
            columns: vec![
                column_def(2, "src", ScalarType::Text),
                column_def(3, "dst", ScalarType::Text),
            ],
            primary_key: vec!["src".into(), "dst".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }]),
        data,
        query,
        expected,
    )
}

fn generated_nullable_ints(choices: &mut Choices<'_>) -> Vec<Option<i64>> {
    (0..choices.range(0, 6))
        .map(|_| {
            if choices.chance(4) {
                None
            } else {
                Some([-1, 0, 1, 2][choices.index(4)])
            }
        })
        .collect()
}

fn int_rows(scope: &str, values: &[Option<i64>]) -> Relation {
    Relation::Rows {
        scope: scope.into(),
        columns: vec![RowsColumn {
            name: "v".into(),
            kind: Kind::Int64,
            nullable: true,
        }],
        values: values
            .iter()
            .map(|value| {
                vec![
                    value
                        .map(|value| RawScalar::Number(value.to_string()))
                        .unwrap_or(RawScalar::Null),
                ]
            })
            .collect(),
    }
}

fn ordered_query(
    relation: Relation,
    scope: &str,
    column_name: &str,
    cardinality: RootCardinality,
) -> Query {
    Query {
        root: Relation::Order {
            input: Box::new(relation),
            terms: vec![OrderTerm {
                expression: column(scope, column_name),
                descending: false,
            }],
        },
        cardinality,
        bindings: HashMap::new(),
    }
}

fn intersect(left: &[Option<i64>], right: &[Option<i64>], distinct: bool) -> Vec<Option<i64>> {
    let mut remaining = right.to_vec();
    let mut output = Vec::new();
    for value in left {
        if let Some(index) = remaining.iter().position(|candidate| candidate == value) {
            if !distinct || !output.contains(value) {
                output.push(*value);
            }
            remaining.remove(index);
        }
    }
    output
}

fn except(left: &[Option<i64>], right: &[Option<i64>], distinct: bool) -> Vec<Option<i64>> {
    if distinct {
        let mut output = Vec::new();
        for value in left {
            if !right.contains(value) && !output.contains(value) {
                output.push(*value);
            }
        }
        return output;
    }
    let mut remaining = right.to_vec();
    let mut output = Vec::new();
    for value in left {
        if let Some(index) = remaining.iter().position(|candidate| candidate == value) {
            remaining.remove(index);
        } else {
            output.push(*value);
        }
    }
    output
}

fn deduplicate(values: &mut Vec<Option<i64>>) {
    let mut output = Vec::new();
    for value in values.drain(..) {
        if !output.contains(&value) {
            output.push(value);
        }
    }
    *values = output;
}

fn aggregate(function: AggregateFunction, name: &str) -> AggregateTerm {
    AggregateTerm {
        function,
        argument: Some(column("r", "v")),
        name: name.into(),
    }
}

fn aggregate_json(group: Option<bool>, values: &[Option<i64>]) -> JsonValue {
    let present = values.iter().flatten().copied().collect::<Vec<_>>();
    let sum = (!present.is_empty()).then(|| present.iter().sum::<i64>());
    let average = (!present.is_empty())
        .then(|| present.iter().map(|value| *value as f64).sum::<f64>() / present.len() as f64);
    let minimum = present.iter().min().copied();
    let maximum = present.iter().max().copied();
    let mut value = json!({
        "rows": values.len() as i64,
        "values": present.len() as i64,
        "sum": sum,
        "avg": average,
        "min": minimum,
        "max": maximum,
    });
    if let Some(group) = group {
        value["g"] = json!(group);
    }
    value
}

fn empty_schema() -> Schema {
    Schema::from_definitions(Vec::new())
}

fn column(scope: &str, name: &str) -> Expr {
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

fn literal_bool(value: bool) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Bool(value),
        kind: Some(Kind::Bool),
    })
}

fn column_def(id: u32, name: &str, scalar_type: ScalarType) -> ColumnDef {
    ColumnDef {
        id: schema_id(id),
        name: name.into(),
        scalar_type,
        nullable: false,
        format: String::new(),
        default: None,
    }
}

fn schema_id(value: u32) -> SchemaId {
    SchemaId::new(value).expect("semantic model schema IDs are positive")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_family_reaches_every_independent_model() {
        let modes = (0..128)
            .map(|seed| SemanticModelCase::from_seed(seed).decisions[0] % 5)
            .collect::<HashSet<_>>();
        assert_eq!(modes, HashSet::from([0, 1, 2, 3, 4]));
    }

    #[test]
    fn distinct_except_excludes_every_left_copy_when_right_contains_the_value() {
        assert_eq!(
            except(&[Some(0), None, Some(0), Some(1)], &[Some(0)], true),
            vec![None, Some(1)]
        );
    }
}
