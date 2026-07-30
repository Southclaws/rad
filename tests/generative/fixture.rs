use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use rad::engine::catalog::model::{ScalarType, Schema};
use rad::engine::lir::{
    AggregateFunction, BinaryOp, Expr, JoinKind, Kind, Query, RawScalar, Relation, RootCardinality,
    SetQuantifier, TextComparison, TextMatchPart, UnaryOp, Value,
};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use super::{Case, TestResult, oracle_json};

pub async fn emit_fixture(case: &Case, master_seed: u64, detail: &str) -> TestResult<PathBuf> {
    let query = wire_query(&case.query)?;
    let encoded = serde_json::to_vec(&query).map_err(|error| error.to_string())?;
    let digest = Sha256::digest(encoded);
    let suffix = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!("bug_generated_{suffix}");
    let root = match env::var("RAD_GEN_EMIT").as_deref() {
        Ok("1") | Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e"),
        Ok(path) => PathBuf::from(path),
    };
    let directory = root.join(&name);
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    fs::write(
        directory.join("rad.schema.yaml"),
        schema_yaml(&case.catalog),
    )
    .map_err(|error| error.to_string())?;

    let seed = case
        .catalog
        .tables
        .iter()
        .filter_map(|table| {
            let rows = case.data.get(&table.name)?;
            (!rows.is_empty()).then(|| {
                json!({
                    "table": table.name,
                    "rows": rows.iter().map(row_json).collect::<Vec<_>>()
                })
            })
        })
        .collect::<Vec<_>>();
    if !seed.is_empty() {
        write_json(&directory.join("seed.json"), &Json::Array(seed))?;
    }

    let replay = json!({
        "format": "rad-generated-case-v1",
        "generator": "splitmix64-decision-tape-v1",
        "revision": env::var("GITHUB_SHA").ok(),
        "package_version": env!("CARGO_PKG_VERSION"),
        "master_seed": master_seed,
        "decisions": case.decisions,
        "campaign": match case.kind {
            super::CaseKind::Relational => "relational",
            super::CaseKind::Recursive => "recursive",
            super::CaseKind::NestedIdentity => "nested_identity",
        },
        "ordered": case.ordered,
        "query": query,
    });
    write_json(&directory.join("generated-case.json"), &replay)?;

    if let Ok(expected) = oracle_json(case).await {
        let test = json!({
            "program": {
                "statements": [{
                    "name": "q",
                    "kind": "query",
                    "relation": query
                }]
            },
            "result": expected
        });
        write_json(&directory.join(format!("test_{name}.json")), &test)?;
    }
    fs::write(
        directory.join("BUG.md"),
        format!(
            "# Generated differential regression\n\nThe native generator found and minimized a four-way divergence.\n\n```text\n{detail}\n```\n\nReplay the exact minimized decision tape and campaign from `generated-case.json` with `RAD_GEN_REPLAY` and the recorded `campaign` as `RAD_GEN_REPLAY_KIND`.\n"
        ),
    )
    .map_err(|error| error.to_string())?;
    Ok(directory)
}

fn write_json(path: &Path, value: &Json) -> TestResult<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| error.to_string())
}

fn row_json(row: &rad::engine::lir::Row) -> Json {
    Json::Object(
        row.iter()
            .map(|(name, value)| (name.clone(), value_json(value)))
            .collect(),
    )
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::Text(value) => Json::String(value.clone()),
        Value::Int64(value) => (*value).into(),
        Value::Float64(value) => json!(value),
        Value::Bool(value) => (*value).into(),
        Value::Null(_) => Json::Null,
    }
}

fn schema_yaml(schema: &Schema) -> String {
    let mut output = String::from("tables:\n");
    for table in &schema.tables {
        output.push_str(&format!(
            "  - id: {}\n    name: {}\n    columns:\n",
            table.id, table.name
        ));
        for column in &table.columns {
            let scalar_type = match column.scalar_type {
                ScalarType::Text => "string",
                ScalarType::Int64 => "int64",
                ScalarType::Float64 => "float64",
                ScalarType::Bool => "bool",
            };
            output.push_str(&format!(
                "      - {{ id: {}, name: {}, type: {}",
                column.id, column.name, scalar_type
            ));
            if table.primary_key.len() == 1 && table.primary_key[0] == column.name {
                output.push_str(", pk: true");
            }
            if column.nullable {
                output.push_str(", nullable: true");
            }
            output.push_str(" }\n");
        }
        if table.primary_key.len() > 1 {
            output.push_str(&format!(
                "    primary_key: [{}]\n",
                table.primary_key.join(", ")
            ));
        }
        if !table.indexes.is_empty() {
            output.push_str("    indexes:\n");
            for index in &table.indexes {
                output.push_str(&format!(
                    "      - {{ name: {}, columns: [{}]{} }}\n",
                    index.name,
                    index.columns.join(", "),
                    if index.unique { ", unique: true" } else { "" }
                ));
            }
        }
        if !table.foreign_keys.is_empty() {
            output.push_str("    foreign_keys:\n");
            for foreign_key in &table.foreign_keys {
                output.push_str(&format!(
                    "      - {{ name: {}, columns: [{}], ref_table: {}, ref_columns: [{}] }}\n",
                    foreign_key.name,
                    foreign_key.columns.join(", "),
                    foreign_key.ref_table,
                    foreign_key.ref_columns.join(", ")
                ));
            }
        }
    }
    output
}

fn wire_query(query: &Query) -> TestResult<Json> {
    let mut builder = WireBuilder::default();
    let root = builder.relation(&query.root)?;
    let mut bindings = serde_json::Map::new();
    let mut names = query.bindings.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        match &query.bindings[name] {
            Relation::Recursive {
                anchor,
                step,
                accumulation,
            } => {
                let anchor = builder.relation(anchor)?;
                let step = builder.relation(step)?;
                bindings.insert(
                    name.clone(),
                    json!({
                        "kind": "recursive",
                        "anchor": anchor,
                        "step": step,
                        "accumulation": match accumulation {
                            rad::engine::lir::RecursiveAccumulation::All => "all",
                            rad::engine::lir::RecursiveAccumulation::New => "new",
                        }
                    }),
                );
            }
            relation => {
                bindings.insert(
                    name.clone(),
                    json!({"kind": "derived", "node": builder.relation(relation)?}),
                );
            }
        }
    }
    let mut value = json!({
        "nodes": builder.nodes,
        "root": {
            "node": root,
            "cardinality": match query.cardinality {
                RootCardinality::Many => "many",
                RootCardinality::First => "first",
                RootCardinality::ExactlyOne => "exactly_one",
                RootCardinality::Scalar => "scalar",
            }
        }
    });
    if !bindings.is_empty() {
        value["bindings"] = Json::Object(bindings);
    }
    Ok(value)
}

#[derive(Default)]
struct WireBuilder {
    nodes: BTreeMap<String, Json>,
    next: usize,
}

impl WireBuilder {
    fn relation(&mut self, relation: &Relation) -> TestResult<String> {
        let node = match relation {
            Relation::Scan { table, scope } => json!({"kind":"scan", "table":table, "scope":scope}),
            Relation::Rows {
                scope,
                columns,
                values,
            } => {
                let columns = columns
                    .iter()
                    .map(|column| {
                        Ok(json!({
                            "name": column.name,
                            "type": kind_name(column.kind)?,
                            "nullable": column.nullable,
                        }))
                    })
                    .collect::<TestResult<Vec<_>>>()?;
                json!({
                    "kind":"rows", "scope":scope, "columns": columns,
                    "rows": values.iter().map(|row| row.iter().map(cell_json).collect::<TestResult<Vec<_>>>()).collect::<TestResult<Vec<_>>>()?,
                })
            }
            Relation::Filter { input, predicate } => json!({
                "kind":"filter", "input":self.relation(input)?, "predicate":self.expression(predicate)?
            }),
            Relation::Project {
                input,
                scope,
                spread,
                fields,
            } => {
                let mut node = json!({
                    "kind":"project", "input":self.relation(input)?,
                    "spread":spread,
                    "fields":fields.iter().map(|field| Ok(json!({"as":field.name, "expr":self.expression(&field.expression)?}))).collect::<TestResult<Vec<_>>>()?
                });
                if let Some(scope) = scope {
                    node["scope"] = json!(scope);
                }
                node
            }
            Relation::Join {
                left,
                right,
                kind,
                on,
            } => json!({
                "kind":"join", "left":self.relation(left)?, "right":self.relation(right)?,
                "join":match kind { JoinKind::Inner => "inner", JoinKind::Left => "left" },
                "on":self.expression(on)?
            }),
            Relation::Concatenate { scope, inputs } => json!({
                "kind":"concatenate", "scope":scope,
                "inputs":inputs.iter().map(|input| self.relation(input)).collect::<TestResult<Vec<_>>>()?
            }),
            Relation::Intersect {
                scope,
                left,
                right,
                quantifier,
            } => json!({
                "kind":"intersect", "scope":scope, "left":self.relation(left)?, "right":self.relation(right)?,
                "quantifier":quantifier_name(*quantifier)
            }),
            Relation::Except {
                scope,
                left,
                right,
                quantifier,
            } => json!({
                "kind":"except", "scope":scope, "left":self.relation(left)?, "right":self.relation(right)?,
                "quantifier":quantifier_name(*quantifier)
            }),
            Relation::Aggregate {
                input,
                scope,
                groups,
                terms,
            } => {
                let mut node = json!({
                    "kind":"aggregate", "input":self.relation(input)?,
                    "groups":groups.iter().map(|group| Ok(json!({"as":group.name, "expr":self.expression(&group.expression)?}))).collect::<TestResult<Vec<_>>>()?,
                    "aggs":terms.iter().map(|term| {
                        let mut value = json!({"fn":aggregate_name(term.function), "as":term.name});
                        if let Some(argument) = &term.argument { value["arg"] = self.expression(argument)?; }
                        Ok(value)
                    }).collect::<TestResult<Vec<_>>>()?
                });
                if let Some(scope) = scope {
                    node["scope"] = json!(scope);
                }
                node
            }
            Relation::Order { input, terms } => json!({
                "kind":"order", "input":self.relation(input)?,
                "terms":terms.iter().map(|term| Ok(json!({"expr":self.expression(&term.expression)?, "desc":term.descending}))).collect::<TestResult<Vec<_>>>()?
            }),
            Relation::Slice {
                input,
                offset,
                limit,
            } => json!({
                "kind":"slice", "input":self.relation(input)?, "offset":offset, "limit":limit
            }),
            Relation::Ref { binding, scope } => {
                json!({"kind":"ref", "binding":binding, "scope":scope})
            }
            Relation::RecursiveRef { binding, scope } => {
                json!({"kind":"recursive_ref", "binding":binding, "scope":scope})
            }
            Relation::Recursive { .. } => {
                return Err("recursive relation is valid only as a binding".into());
            }
            Relation::Distinct(input) => json!({"kind":"distinct", "input":self.relation(input)?}),
        };
        self.next += 1;
        let name = format!("n{}", self.next);
        self.nodes.insert(name.clone(), node);
        Ok(name)
    }

    fn expression(&mut self, expression: &Expr) -> TestResult<Json> {
        Ok(match expression {
            Expr::Literal(literal) => {
                let kind = literal
                    .kind
                    .or(match literal.raw {
                        RawScalar::Text(_) => Some(Kind::Text),
                        // Raw non-null numbers deliberately lose their int/float
                        // distinction after lowering, so fixture generation is
                        // only guaranteed for the original generated case.
                        RawScalar::Number(_) | RawScalar::Null => None,
                        RawScalar::Bool(_) => Some(Kind::Bool),
                    })
                    .ok_or_else(|| "literal has no recoverable wire type".to_string())?;
                let mut value = json!({"type":kind_name(kind)?});
                match &literal.raw {
                    RawScalar::Null => {}
                    RawScalar::Text(text) | RawScalar::Number(text) => value["value"] = json!(text),
                    RawScalar::Bool(boolean) => value["value"] = json!(boolean),
                }
                json!({"kind":"lit", "value":value})
            }
            Expr::Column { scope, name } => json!({"kind":"col", "scope":scope, "column":name}),
            Expr::Unary { op, expression } => {
                json!({"kind":"unary", "op":unary_name(*op), "expr":self.expression(expression)?})
            }
            Expr::Binary { op, left, right } => {
                json!({"kind":"binary", "op":binary_name(*op), "left":self.expression(left)?, "right":self.expression(right)?})
            }
            Expr::Cast { expression, to } => {
                json!({"kind":"cast", "expr":self.expression(expression)?, "to":kind_name(*to)?})
            }
            Expr::Branch { arms, otherwise } => json!({
                "kind":"branch", "branches":arms.iter().map(|arm| Ok(json!({"when":self.expression(&arm.when)?, "then":self.expression(&arm.then)?}))).collect::<TestResult<Vec<_>>>()?,
                "else":self.expression(otherwise)?
            }),
            Expr::TextMatch {
                value,
                parts,
                comparison,
            } => json!({
                "kind":"text_match", "value":self.expression(value)?,
                "comparison":match comparison { TextComparison::Exact => "exact", TextComparison::UnicodeSimpleFold => "unicode_simple_fold" },
                "parts":parts.iter().map(|part| match part { TextMatchPart::Literal(value) => json!({"kind":"literal", "value":value}), TextMatchPart::AnyMany => json!({"kind":"any_many"}) }).collect::<Vec<_>>()
            }),
            Expr::Exists(relation) => json!({"kind":"exists", "node":self.relation(relation)?}),
            Expr::First(relation) => json!({"kind":"first", "node":self.relation(relation)?}),
            Expr::Scalar(relation) => json!({"kind":"scalar", "node":self.relation(relation)?}),
            Expr::Array(relation) => json!({"kind":"array", "node":self.relation(relation)?}),
        })
    }
}

fn kind_name(kind: Kind) -> TestResult<&'static str> {
    match kind {
        Kind::Text => Ok("text"),
        Kind::Int64 => Ok("int64"),
        Kind::Float64 => Ok("float64"),
        Kind::Bool => Ok("bool"),
        Kind::Row | Kind::Array => Err(format!("{kind} is not a wire scalar")),
    }
}

fn cell_json(value: &RawScalar) -> TestResult<Json> {
    Ok(match value {
        RawScalar::Null => Json::Null,
        RawScalar::Text(value) | RawScalar::Number(value) => json!(value),
        RawScalar::Bool(value) => json!(value.to_string()),
    })
}

fn quantifier_name(value: SetQuantifier) -> &'static str {
    match value {
        SetQuantifier::All => "all",
        SetQuantifier::Distinct => "distinct",
    }
}
fn aggregate_name(value: AggregateFunction) -> &'static str {
    match value {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Average => "avg",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
    }
}
fn unary_name(value: UnaryOp) -> &'static str {
    match value {
        UnaryOp::Not => "not",
        UnaryOp::Negate => "negate",
        UnaryOp::IsNull => "is_null",
        UnaryOp::IsNotNull => "is_not_null",
    }
}
fn binary_name(value: BinaryOp) -> &'static str {
    match value {
        BinaryOp::Eq => "eq",
        BinaryOp::Ne => "ne",
        BinaryOp::Lt => "lt",
        BinaryOp::Lte => "lte",
        BinaryOp::Gt => "gt",
        BinaryOp::Gte => "gte",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Add => "add",
        BinaryOp::Sub => "sub",
        BinaryOp::Mul => "mul",
        BinaryOp::Div => "div",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_query_round_trips_through_normative_wire_types() {
        let case = Case::from_seed(7);
        let value = wire_query(&case.query).unwrap();
        let wire = serde_json::from_value(value).unwrap();
        let _lowered = rad::protocol::lower_lir(wire).unwrap();
    }

    #[test]
    fn emitted_query_contains_only_the_reachable_root_graph() {
        let query = Query {
            root: Relation::Scan {
                table: "items".into(),
                scope: "item".into(),
            },
            cardinality: RootCardinality::Many,
            bindings: std::collections::HashMap::new(),
        };
        let value = wire_query(&query).unwrap();
        assert_eq!(value["nodes"].as_object().unwrap().len(), 1);
        assert!(
            value["nodes"]
                .get(value["root"]["node"].as_str().unwrap())
                .is_some()
        );
    }

    #[test]
    fn emitted_recursive_query_and_schema_are_directly_replayable() {
        let case = super::super::recursive_case(9);
        let value = wire_query(&case.query).unwrap();
        let wire = serde_json::from_value(value).unwrap();
        let _lowered = rad::protocol::lower_lir(wire).unwrap();
        let schema = schema_yaml(&case.catalog);
        rad::engine::catalog::schema::parse("generated.yaml", schema.as_bytes()).unwrap();
    }
}
