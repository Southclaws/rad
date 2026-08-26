use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::engine::lir::{
    AggregateTerm, BranchArm, Expr, GroupTerm, OrderTerm, ProjectField, Query, RawScalar, Relation,
    TextMatchPart,
};

use super::{DefaultSpec, Program, Statement};

pub const MAX_PROGRAM_DOCUMENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramFingerprints {
    pub exact: String,
    pub family: String,
}

pub fn program_fingerprints(program: &Program) -> ProgramFingerprints {
    ProgramFingerprints {
        exact: fingerprint("exact", &program_value(program, false)),
        family: fingerprint("family", &program_value(program, true)),
    }
}

pub fn program_document(program: &Program) -> Vec<u8> {
    serde_json::to_vec(&program_value(program, false)).unwrap_or_default()
}

pub fn program_family_document(program: &Program) -> Vec<u8> {
    serde_json::to_vec(&program_value(program, true)).unwrap_or_default()
}

fn fingerprint(form: &str, value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("p1:{form}:sha256:{digest:x}")
}

fn program_value(program: &Program, family: bool) -> Value {
    json!({
        "statements": program
            .statements
            .iter()
            .map(|statement| statement_value(statement, family))
            .collect::<Vec<_>>(),
        "result": program.result,
    })
}

fn statement_value(statement: &Statement, family: bool) -> Value {
    match statement {
        Statement::Query { name, relation } => {
            relational_statement(name, "query", relation, family)
        }
        Statement::Create {
            name,
            relation,
            table,
        } => json!({
            "name": name,
            "kind": "create",
            "table": table,
            "relation": query_value(relation, family),
        }),
        Statement::Update {
            name,
            relation,
            table,
        } => json!({
            "name": name,
            "kind": "update",
            "table": table,
            "relation": query_value(relation, family),
        }),
        Statement::Delete {
            name,
            relation,
            table,
        } => json!({
            "name": name,
            "kind": "delete",
            "table": table,
            "relation": query_value(relation, family),
        }),
        Statement::CreateTable { name, table } => json!({
            "name": name,
            "kind": "create_table",
            "table": table_draft_value(table, family),
        }),
        Statement::RenameTable { name, table_id, to } => json!({
            "name": name,
            "kind": "rename_table",
            "table_id": table_id,
            "to": to,
        }),
        Statement::DeleteTable { name, table_id } => json!({
            "name": name,
            "kind": "delete_table",
            "table_id": table_id,
        }),
        Statement::CreateColumn {
            name,
            table_id,
            column,
        } => json!({
            "name": name,
            "kind": "create_column",
            "table_id": table_id,
            "column": column_draft_value(column, family),
        }),
        Statement::RenameColumn {
            name,
            table_id,
            column_id,
            to,
        } => json!({
            "name": name,
            "kind": "rename_column",
            "table_id": table_id,
            "column_id": column_id,
            "to": to,
        }),
        Statement::ChangeColumnDefault {
            name,
            table_id,
            column_id,
            default,
        } => json!({
            "name": name,
            "kind": "change_column_default",
            "table_id": table_id,
            "column_id": column_id,
            "default": default.as_ref().map(|default| default_spec_value(default, family)),
        }),
        Statement::DeleteColumn {
            name,
            table_id,
            column_id,
        } => json!({
            "name": name,
            "kind": "delete_column",
            "table_id": table_id,
            "column_id": column_id,
        }),
        Statement::CreateIndex {
            name,
            table_id,
            index,
        } => json!({
            "name": name,
            "kind": "create_index",
            "table_id": table_id,
            "index": index,
        }),
        Statement::DeleteIndex {
            name,
            table_id,
            index,
        } => json!({
            "name": name,
            "kind": "delete_index",
            "table_id": table_id,
            "index": index,
        }),
        Statement::StartIndexBuild {
            name,
            table_id,
            index,
            prerequisites,
            after,
        } => json!({
            "name": name,
            "kind": "start_index_build",
            "table_id": table_id,
            "index": index,
            "prerequisites": prerequisites,
            "after": after,
        }),
        Statement::StartColumnReplacement {
            name,
            table_id,
            column_id,
            replacement,
            after,
        } => json!({
            "name": name,
            "kind": "start_column_replacement",
            "table_id": table_id,
            "column_id": column_id,
            "replacement": {
                "type": replacement.scalar_type,
                "nullable": replacement.nullable,
                "format": replacement.format,
                "default": default_value(
                    replacement.default.as_ref(),
                    replacement.scalar_type,
                    family,
                ),
                "conversion": replacement.conversion,
                "prerequisites": replacement.prerequisites,
            },
            "after": after,
        }),
        Statement::StartConstraintValidation {
            name,
            table_id,
            constraint,
            after,
        } => json!({
            "name": name,
            "kind": "start_constraint_validation",
            "table_id": table_id,
            "constraint": {
                "name": constraint.name,
                "kind": constraint.kind,
                "column_id": constraint.column_id,
                "prerequisites": constraint.prerequisites,
            },
            "after": after,
        }),
    }
}

fn relational_statement(name: &str, kind: &str, query: &Query, family: bool) -> Value {
    json!({
        "name": name,
        "kind": kind,
        "relation": query_value(query, family),
    })
}

fn query_value(query: &Query, family: bool) -> Value {
    let bindings = query
        .bindings
        .iter()
        .map(|(name, relation)| (name.clone(), relation_value(relation, family)))
        .collect::<BTreeMap<_, _>>();
    json!({
        "root": relation_value(&query.root, family),
        "cardinality": query.cardinality.as_str(),
        "bindings": bindings,
    })
}

fn table_draft_value(table: &crate::engine::catalog::model::TableDraft, family: bool) -> Value {
    json!({
        "id": table.id,
        "name": table.name,
        "columns": table
            .columns
            .iter()
            .map(|column| column_draft_value(column, family))
            .collect::<Vec<_>>(),
        "primary_key": table.primary_key,
        "indexes": table.indexes,
        "foreign_keys": table.foreign_keys,
    })
}

fn column_draft_value(column: &crate::engine::catalog::model::ColumnDraft, family: bool) -> Value {
    json!({
        "id": column.id,
        "name": column.name,
        "type": column.scalar_type,
        "nullable": column.nullable,
        "format": column.format,
        "default": default_value(column.default.as_ref(), column.scalar_type, family),
    })
}

fn default_value(
    default: Option<&crate::engine::catalog::model::DefaultValue>,
    scalar_type: crate::engine::catalog::model::ScalarType,
    family: bool,
) -> Value {
    let Some(default) = default else {
        return Value::Null;
    };
    if !family {
        return serde_json::to_value(default).unwrap_or_default();
    }
    match default.function {
        Some(function) => json!({"function": function}),
        None => json!({
            "value": format!("?{}", format!("{scalar_type:?}").to_lowercase()),
        }),
    }
}

fn default_spec_value(default: &DefaultSpec, family: bool) -> Value {
    match default {
        DefaultSpec::Generator(function) => json!({"function": function}),
        DefaultSpec::Text(value) if !family => json!({"text": value}),
        DefaultSpec::Number(value) if !family => json!({"number": value}),
        DefaultSpec::Bool(value) if !family => json!({"bool": value}),
        DefaultSpec::Text(_) => json!({"text": "?text"}),
        DefaultSpec::Number(_) => json!({"number": "?number"}),
        DefaultSpec::Bool(_) => json!({"bool": "?bool"}),
    }
}

fn relation_value(relation: &Relation, family: bool) -> Value {
    match relation {
        Relation::Scan { table, scope } => json!({"kind": "scan", "table": table, "scope": scope}),
        Relation::Rows {
            scope,
            columns,
            values,
        } => json!({
            "kind": "rows",
            "scope": scope,
            "columns": columns.iter().map(|column| json!({
                "name": column.name,
                "kind": format!("{:?}", column.kind).to_lowercase(),
                "nullable": column.nullable,
            })).collect::<Vec<_>>(),
            "values": values.iter().map(|row| row.iter().enumerate().map(|(index, value)| {
                raw_value_with_kind(value, family, columns.get(index).map(|column| column.kind))
            }).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Relation::Filter { input, predicate } => json!({
            "kind": "filter",
            "input": relation_value(input, family),
            "predicate": expr_value(predicate, family),
        }),
        Relation::Project {
            input,
            scope,
            spread,
            fields,
        } => json!({
            "kind": "project",
            "input": relation_value(input, family),
            "scope": scope,
            "spread": spread,
            "fields": fields.iter().map(|field| project_field_value(field, family)).collect::<Vec<_>>(),
        }),
        Relation::Join {
            left,
            right,
            kind,
            on,
        } => json!({
            "kind": "join",
            "join_kind": kind.as_str(),
            "left": relation_value(left, family),
            "right": relation_value(right, family),
            "on": expr_value(on, family),
        }),
        Relation::Concatenate { scope, inputs } => json!({
            "kind": "concatenate",
            "scope": scope,
            "inputs": inputs.iter().map(|input| relation_value(input, family)).collect::<Vec<_>>(),
        }),
        Relation::Intersect {
            scope,
            left,
            right,
            quantifier,
        } => json!({
            "kind": "intersect",
            "scope": scope,
            "quantifier": quantifier.as_str(),
            "left": relation_value(left, family),
            "right": relation_value(right, family),
        }),
        Relation::Except {
            scope,
            left,
            right,
            quantifier,
        } => json!({
            "kind": "except",
            "scope": scope,
            "quantifier": quantifier.as_str(),
            "left": relation_value(left, family),
            "right": relation_value(right, family),
        }),
        Relation::Aggregate {
            input,
            scope,
            groups,
            terms,
        } => json!({
            "kind": "aggregate",
            "input": relation_value(input, family),
            "scope": scope,
            "groups": groups.iter().map(|group| group_value(group, family)).collect::<Vec<_>>(),
            "terms": terms.iter().map(|term| aggregate_value(term, family)).collect::<Vec<_>>(),
        }),
        Relation::Order { input, terms } => json!({
            "kind": "order",
            "input": relation_value(input, family),
            "terms": terms.iter().map(|term| order_value(term, family)).collect::<Vec<_>>(),
        }),
        Relation::Slice {
            input,
            offset,
            limit,
        } => json!({
            "kind": "slice",
            "input": relation_value(input, family),
            "offset": offset,
            "limit": limit,
        }),
        Relation::Ref { binding, scope } => {
            json!({"kind": "ref", "binding": binding, "scope": scope})
        }
        Relation::RecursiveRef { binding, scope } => {
            json!({"kind": "recursive_ref", "binding": binding, "scope": scope})
        }
        Relation::Recursive {
            anchor,
            step,
            accumulation,
        } => json!({
            "kind": "recursive",
            "anchor": relation_value(anchor, family),
            "step": relation_value(step, family),
            "accumulation": accumulation.as_str(),
        }),
        Relation::Distinct(input) => {
            json!({"kind": "distinct", "input": relation_value(input, family)})
        }
    }
}

fn expr_value(expression: &Expr, family: bool) -> Value {
    match expression {
        Expr::Literal(literal) => json!({
            "kind": "literal",
            "value": raw_value_with_kind(&literal.raw, family, literal.kind),
            "type": literal.kind.map(|kind| format!("{kind:?}").to_lowercase()),
        }),
        Expr::Column { scope, name } => json!({"kind": "column", "scope": scope, "name": name}),
        Expr::Unary { op, expression } => json!({
            "kind": "unary",
            "op": format!("{op:?}").to_lowercase(),
            "expression": expr_value(expression, family),
        }),
        Expr::Binary { op, left, right } => json!({
            "kind": "binary",
            "op": format!("{op:?}").to_lowercase(),
            "left": expr_value(left, family),
            "right": expr_value(right, family),
        }),
        Expr::Cast { expression, to } => json!({
            "kind": "cast",
            "to": format!("{to:?}").to_lowercase(),
            "expression": expr_value(expression, family),
        }),
        Expr::Branch { arms, otherwise } => json!({
            "kind": "branch",
            "arms": arms.iter().map(|arm| branch_value(arm, family)).collect::<Vec<_>>(),
            "otherwise": expr_value(otherwise, family),
        }),
        Expr::TextMatch {
            value,
            parts,
            comparison,
        } => json!({
            "kind": "text_match",
            "value": expr_value(value, family),
            "parts": parts.iter().map(|part| text_match_value(part, family)).collect::<Vec<_>>(),
            "comparison": format!("{comparison:?}").to_lowercase(),
        }),
        Expr::Exists(relation) => crossing_value("exists", relation, family),
        Expr::First(relation) => crossing_value("first", relation, family),
        Expr::Scalar(relation) => crossing_value("scalar", relation, family),
        Expr::Array(relation) => crossing_value("array", relation, family),
    }
}

fn raw_value_with_kind(
    value: &RawScalar,
    family: bool,
    family_kind: Option<crate::engine::lir::Kind>,
) -> Value {
    if family {
        return Value::String(match family_kind {
            Some(kind) => format!("?{kind}"),
            None => match value {
                RawScalar::Null => "?null".to_owned(),
                RawScalar::Text(_) => "?text".to_owned(),
                RawScalar::Number(_) => "?number".to_owned(),
                RawScalar::Bool(_) => "?bool".to_owned(),
            },
        });
    }
    match value {
        RawScalar::Null => Value::Null,
        RawScalar::Text(value) | RawScalar::Number(value) => Value::String(value.clone()),
        RawScalar::Bool(value) => Value::Bool(*value),
    }
}

fn project_field_value(field: &ProjectField, family: bool) -> Value {
    json!({"name": field.name, "expression": expr_value(&field.expression, family)})
}

fn group_value(group: &GroupTerm, family: bool) -> Value {
    json!({"name": group.name, "expression": expr_value(&group.expression, family)})
}

fn aggregate_value(term: &AggregateTerm, family: bool) -> Value {
    json!({
        "name": term.name,
        "function": term.function.as_str(),
        "argument": term.argument.as_ref().map(|argument| expr_value(argument, family)),
    })
}

fn order_value(term: &OrderTerm, family: bool) -> Value {
    json!({"expression": expr_value(&term.expression, family), "descending": term.descending})
}

fn branch_value(arm: &BranchArm, family: bool) -> Value {
    json!({"when": expr_value(&arm.when, family), "then": expr_value(&arm.then, family)})
}

fn text_match_value(part: &TextMatchPart, family: bool) -> Value {
    match part {
        TextMatchPart::Literal(_) if family => json!({"kind": "literal", "value": "?text"}),
        TextMatchPart::Literal(value) => json!({"kind": "literal", "value": value}),
        TextMatchPart::AnyMany => json!({"kind": "any_many"}),
    }
}

fn crossing_value(kind: &str, relation: &Relation, family: bool) -> Value {
    json!({"kind": kind, "relation": relation_value(relation, family)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lir::{Kind, Literal, RootCardinality};

    fn program(value: &str) -> Program {
        Program {
            statements: vec![Statement::Query {
                name: "read".to_owned(),
                relation: Query {
                    root: Relation::Filter {
                        input: Box::new(Relation::Scan {
                            table: "users".to_owned(),
                            scope: "user".to_owned(),
                        }),
                        predicate: Expr::Literal(Literal {
                            raw: RawScalar::Text(value.to_owned()),
                            kind: Some(Kind::Text),
                        }),
                    },
                    cardinality: RootCardinality::Many,
                    bindings: std::collections::HashMap::new(),
                },
            }],
            result: Some("read".to_owned()),
        }
    }

    #[test]
    fn family_fingerprint_ignores_literals() {
        let left = program("left");
        let right = program("right");
        assert_ne!(
            program_fingerprints(&left).exact,
            program_fingerprints(&right).exact
        );
        assert_eq!(
            program_fingerprints(&left).family,
            program_fingerprints(&right).family
        );
    }

    #[test]
    fn catalog_family_fingerprint_preserves_shape() {
        let program = |value: &str, column_id: u32| Program {
            statements: vec![Statement::ChangeColumnDefault {
                name: "change_default".to_owned(),
                table_id: crate::engine::catalog::identity::SchemaId::new(1).unwrap(),
                column_id: crate::engine::catalog::identity::SchemaId::new(column_id).unwrap(),
                default: Some(DefaultSpec::Text(value.to_owned())),
            }],
            result: None,
        };
        let left = program("left", 2);
        let right = program("right", 2);
        let other_column = program("left", 3);
        assert_ne!(
            program_fingerprints(&left).exact,
            program_fingerprints(&right).exact
        );
        assert_eq!(
            program_fingerprints(&left).family,
            program_fingerprints(&right).family
        );
        assert_ne!(
            program_fingerprints(&left).family,
            program_fingerprints(&other_column).family
        );
    }

    #[test]
    fn family_fingerprint_preserves_program_shape() {
        let first = program("value");
        let mut second_statement = first.statements[0].clone();
        if let Statement::Query { name, .. } = &mut second_statement {
            *name = "second".to_owned();
        }
        let ordered = Program {
            statements: vec![first.statements[0].clone(), second_statement.clone()],
            result: Some("read".to_owned()),
        };
        let reversed = Program {
            statements: vec![second_statement, first.statements[0].clone()],
            result: Some("read".to_owned()),
        };
        assert_ne!(
            program_fingerprints(&ordered).family,
            program_fingerprints(&reversed).family
        );

        let mut selected = ordered.clone();
        selected.result = Some("second".to_owned());
        assert_ne!(
            program_fingerprints(&ordered).family,
            program_fingerprints(&selected).family
        );
    }

    #[test]
    fn binding_map_order_does_not_change_fingerprints() {
        let relation = Relation::Scan {
            table: "users".to_owned(),
            scope: "user".to_owned(),
        };
        let mut left = program("value");
        let Statement::Query {
            relation: left_query,
            ..
        } = &mut left.statements[0]
        else {
            unreachable!()
        };
        left_query.bindings.insert("a".to_owned(), relation.clone());
        left_query.bindings.insert("b".to_owned(), relation.clone());
        let mut right = program("value");
        let Statement::Query {
            relation: right_query,
            ..
        } = &mut right.statements[0]
        else {
            unreachable!()
        };
        right_query
            .bindings
            .insert("b".to_owned(), relation.clone());
        right_query.bindings.insert("a".to_owned(), relation);
        assert_eq!(program_fingerprints(&left), program_fingerprints(&right));
    }
}
