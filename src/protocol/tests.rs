use super::generated::{lir, pir};

#[test]
fn lir_uses_closed_tagged_unions_and_closed_objects() {
    let unknown_variant = r#"{
        "nodes": {"source": {"kind": "unknown"}},
        "root": {"node": "source", "cardinality": "many"}
    }"#;
    assert!(serde_json::from_str::<lir::Query>(unknown_variant).is_err());

    let unknown_field = r#"{
        "nodes": {
            "source": {
                "kind": "scan",
                "table": "people",
                "scope": "person",
                "unexpected": true
            }
        },
        "root": {"node": "source", "cardinality": "many"}
    }"#;
    assert!(serde_json::from_str::<lir::Query>(unknown_field).is_err());
}

#[test]
fn lir_wire_rejects_every_malformed_structural_family() {
    for (name, raw) in [
        (
            "missing nodes",
            r#"{"root":{"node":"s","cardinality":"many"}}"#,
        ),
        (
            "premature version field",
            r#"{"lir_version":1,"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"}},"root":{"node":"s","cardinality":"many"}}"#,
        ),
        (
            "unknown query field",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"}},"root":{"node":"s","cardinality":"many"},"extra":true}"#,
        ),
        (
            "field from another node variant",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s","predicate":{"kind":"lit","value":{"type":"bool","value":"true"}}}},"root":{"node":"s","cardinality":"many"}}"#,
        ),
        (
            "missing filter predicate",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s"}},"root":{"node":"f","cardinality":"many"}}"#,
        ),
        (
            "missing binary operand",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"binary","op":"eq","left":{"kind":"lit","value":{"type":"int64","value":"1"}}}}},"root":{"node":"f","cardinality":"many"}}"#,
        ),
        (
            "unknown binary operator",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"binary","op":"wat","left":{"kind":"lit","value":{"type":"int64","value":"1"}},"right":{"kind":"lit","value":{"type":"int64","value":"1"}}}}},"root":{"node":"f","cardinality":"many"}}"#,
        ),
        (
            "removed call expression",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"call","fn":"lower","args":[]}}},"root":{"node":"f","cardinality":"many"}}"#,
        ),
        (
            "unknown text comparison",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"text_match","value":{"kind":"col","scope":"s","column":"name"},"comparison":"locale_magic","parts":[{"kind":"literal","value":"x"}]}}},"root":{"node":"f","cardinality":"many"}}"#,
        ),
    ] {
        assert!(
            serde_json::from_str::<lir::Query>(raw).is_err(),
            "accepted malformed LIR family {name}"
        );
    }

    let negative_slice = r#"{
        "nodes": {
            "s": {"kind": "scan", "table": "tasks", "scope": "s"},
            "p": {"kind": "slice", "input": "s", "limit": -1}
        },
        "root": {"node": "p", "cardinality": "many"}
    }"#;
    let wire = serde_json::from_str::<lir::Query>(negative_slice).unwrap();
    assert!(super::lower_lir(wire).is_err());
}

#[test]
fn pir_wire_is_a_closed_statement_union_and_preserves_catalog_variants() {
    for raw in [
        r#"{"statements":[{"kind":"upsert","name":"x","table":"t","relation":{}}]}"#,
        r#"{"statements":[{"kind":"rename_table","name":"rename","to":"accounts"}]}"#,
        r#"{"statements":[{"kind":"inspect_schema_transition","name":"inspect","transition_id":"tr42"}]}"#,
        r#"{"statements":[{"kind":"cancel_schema_transition","name":"cancel","transition_id":"tr42"}]}"#,
    ] {
        assert!(serde_json::from_str::<pir::Program>(raw).is_err(), "{raw}");
    }

    let raw = r#"{
        "statements": [
            {
                "kind": "change_column_default",
                "name": "set",
                "table_id": 7,
                "column_id": 3,
                "default": {"kind": "literal", "value": 9007199254740993}
            },
            {
                "kind": "start_index_build",
                "name": "build",
                "table_id": 7,
                "index": {"name": "users_email", "columns": ["email"]},
                "prerequisites": ["tr39"],
                "after": ["replace"]
            }
        ]
    }"#;
    let program = serde_json::from_str::<pir::Program>(raw).unwrap();
    let encoded = serde_json::to_string(&program).unwrap();
    assert!(encoded.contains("9007199254740993"));
    assert!(encoded.contains("tr39"));
    assert!(encoded.contains("replace"));
    assert_eq!(
        serde_json::from_str::<pir::Program>(&encoded).unwrap(),
        program
    );
}

#[test]
fn omitted_text_match_comparison_lowers_to_exact() {
    let raw = r#"{
        "nodes": {
            "row": {
                "kind": "rows",
                "scope": "row",
                "columns": [{"name": "name", "type": "text"}],
                "rows": [["Rad"]]
            },
            "matched": {
                "kind": "filter",
                "input": "row",
                "predicate": {
                    "kind": "text_match",
                    "value": {"kind": "col", "scope": "row", "column": "name"},
                    "parts": [{"kind": "literal", "value": "Rad"}]
                }
            }
        },
        "root": {"node": "matched", "cardinality": "many"}
    }"#;
    let wire = serde_json::from_str::<lir::Query>(raw).unwrap();
    let encoded = serde_json::to_string(&wire).unwrap();
    assert!(!encoded.contains("comparison"));
    let lowered = super::lower_lir(wire).unwrap();
    let crate::engine::lir::Relation::Filter { predicate, .. } = lowered.root else {
        panic!("expected filter root")
    };
    assert!(matches!(
        predicate,
        crate::engine::lir::Expr::TextMatch {
            comparison: crate::engine::lir::TextComparison::Exact,
            ..
        }
    ));
}

#[test]
fn optional_non_nullable_fields_reject_explicit_null() {
    let missing = r#"{"statements": []}"#;
    let program = serde_json::from_str::<pir::Program>(missing).expect("missing result is valid");
    assert!(program.result.is_missing());

    let explicit_null = r#"{"statements": [], "result": null}"#;
    assert!(serde_json::from_str::<pir::Program>(explicit_null).is_err());
}

#[test]
fn nullable_values_retain_null_as_a_distinct_state() {
    let null = serde_json::from_str::<lir::Cell>("null").expect("cell accepts null");
    assert_eq!(null.as_ref(), None);

    let value = serde_json::from_str::<lir::Cell>(r#""hello""#).expect("cell accepts text");
    assert_eq!(value.as_ref().map(String::as_str), Some("hello"));
}

#[test]
fn pir_relations_preserve_raw_json_lexically() {
    let raw = r#"{
        "statements": [{
            "kind": "query",
            "name": "large_integer",
            "relation": {"literal": 9007199254740993}
        }]
    }"#;
    let program = serde_json::from_str::<pir::Program>(raw).expect("program decodes");
    let pir::Statement::QueryStatement(statement) = &program.statements[0] else {
        panic!("expected query statement");
    };
    assert!(statement.relation.as_str().contains("9007199254740993"));

    let encoded = serde_json::to_string(&program).expect("program encodes");
    assert!(encoded.contains("9007199254740993"));
    let decoded = serde_json::from_str::<pir::Program>(&encoded).expect("round trip decodes");
    assert_eq!(decoded, program);
}

#[test]
fn representative_lir_decodes_to_typed_variants_and_enums() {
    let raw = r#"{
        "nodes": {
            "source": {"kind": "scan", "table": "people", "scope": "person"},
            "sorted": {
                "kind": "order",
                "input": "source",
                "terms": [{
                    "expr": {"kind": "col", "scope": "person", "column": "name"}
                }]
            }
        },
        "root": {"node": "sorted", "cardinality": "many"}
    }"#;
    let query = serde_json::from_str::<lir::Query>(raw).expect("query decodes");
    assert_eq!(query.root.cardinality, lir::RootCardinality::Many);
    assert!(matches!(query.nodes["source"], lir::Node::ScanNode(_)));
    assert!(matches!(query.nodes["sorted"], lir::Node::OrderNode(_)));
}

#[test]
fn lowering_rejects_shared_wire_nodes_before_binding() {
    let raw = r#"{
        "nodes": {
            "row": {
                "kind": "rows",
                "scope": "row",
                "columns": [{"name": "id", "type": "text"}],
                "rows": [["a"]]
            },
            "both": {
                "kind": "concatenate",
                "scope": "both",
                "inputs": ["row", "row"]
            }
        },
        "root": {"node": "both", "cardinality": "many"}
    }"#;
    let query = serde_json::from_str::<lir::Query>(raw).unwrap();
    let error = super::lower_lir(query).unwrap_err();
    assert_eq!(error.reason(), crate::engine::exec::ErrorReason::SharedNode);
    assert!(error.to_string().contains("more than one consumer"));
    assert!(error.to_string().contains("duplicate scope"));
}

#[test]
fn lowering_preserves_non_null_numeric_literal_types() {
    let raw = r#"{
        "nodes": {
            "row": {
                "kind": "rows",
                "scope": "row",
                "columns": [{"name": "id", "type": "text"}],
                "rows": [["a"]]
            },
            "projected": {
                "kind": "project",
                "input": "row",
                "scope": "projected",
                "fields": [{
                    "as": "zero",
                    "expr": {
                        "kind": "lit",
                        "value": {"type": "float64", "value": "0"}
                    }
                }]
            }
        },
        "root": {"node": "projected", "cardinality": "many"}
    }"#;
    let wire = serde_json::from_str::<lir::Query>(raw).expect("query decodes");
    let query = super::lower_lir(wire).expect("query lowers");
    let crate::engine::lir::Relation::Project { fields, .. } = query.root else {
        panic!("expected project root");
    };
    let crate::engine::lir::Expr::Literal(literal) = &fields[0].expression else {
        panic!("expected literal projection");
    };
    assert_eq!(literal.kind, Some(crate::engine::lir::Kind::Float64));
}

#[test]
fn lowering_rejection_reasons_are_stable() {
    use crate::engine::exec::ErrorReason;

    let cases = [
        (
            "missing root reference",
            r#"{"nodes":{},"root":{"node":"","cardinality":"many"}}"#,
            ErrorReason::SchemaViolation,
            "missing node reference",
        ),
        (
            "unknown root node",
            r#"{"nodes":{},"root":{"node":"missing","cardinality":"many"}}"#,
            ErrorReason::UnknownNode,
            "unknown node",
        ),
        (
            "node cycle",
            r#"{"nodes":{"a":{"kind":"filter","input":"b","predicate":{"kind":"lit","value":{"type":"bool","value":true}}},"b":{"kind":"filter","input":"a","predicate":{"kind":"lit","value":{"type":"bool","value":true}}}},"root":{"node":"a","cardinality":"many"}}"#,
            ErrorReason::NodeCycle,
            "part of a cycle",
        ),
        (
            "unreachable node",
            r#"{"nodes":{"root":{"kind":"scan","table":"tasks","scope":"root"},"orphan":{"kind":"scan","table":"tasks","scope":"orphan"}},"root":{"node":"root","cardinality":"many"}}"#,
            ErrorReason::UnreachableNode,
            "unreachable node definitions",
        ),
        (
            "row arity",
            r#"{"nodes":{"r":{"kind":"rows","scope":"r","columns":[{"name":"a","type":"text"},{"name":"b","type":"text"}],"rows":[["one"]]}},"root":{"node":"r","cardinality":"many"}}"#,
            ErrorReason::SchemaViolation,
            "has 1 cells, want 2",
        ),
        (
            "invalid bool cell",
            r#"{"nodes":{"r":{"kind":"rows","scope":"r","columns":[{"name":"ready","type":"bool"}],"rows":[["yes"]]}},"root":{"node":"r","cardinality":"many"}}"#,
            ErrorReason::SchemaViolation,
            "invalid bool payload",
        ),
        (
            "negative slice offset",
            r#"{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"p":{"kind":"slice","input":"s","offset":-1}},"root":{"node":"p","cardinality":"many"}}"#,
            ErrorReason::SchemaViolation,
            "slice offset must be non-negative",
        ),
    ];

    for (name, raw, reason, detail) in cases {
        let wire = serde_json::from_str::<lir::Query>(raw)
            .unwrap_or_else(|error| panic!("case {name} did not reach lowering: {error}"));
        let error = match super::lower_lir(wire) {
            Ok(_) => panic!("case {name} unexpectedly lowered"),
            Err(error) => error,
        };
        assert_eq!(error.reason(), reason, "case {name}: {error}");
        assert!(error.to_string().contains(detail), "case {name}: {error}");
    }
}

#[test]
fn pir_lowering_rejects_invalid_embedded_contracts_by_reason() {
    use crate::engine::exec::ErrorReason;

    let cases = [
        (
            "invalid embedded LIR",
            r#"{"statements":[{"kind":"query","name":"q","relation":{"nodes":{},"root":{"node":"","cardinality":"many"}}}]}"#,
            "missing node reference",
        ),
        (
            "zero schema ID",
            r#"{"statements":[{"kind":"rename_table","name":"rename","table_id":0,"to":"renamed"}]}"#,
            "schema ID",
        ),
        (
            "unsupported constraint kind",
            r#"{"statements":[{"kind":"start_constraint_validation","name":"validate","table_id":1,"constraint":{"name":"check","kind":"check","column_id":1}}]}"#,
            "unsupported constraint kind",
        ),
        (
            "non-scalar literal default",
            r#"{"statements":[{"kind":"change_column_default","name":"default","table_id":1,"column_id":1,"default":{"kind":"literal","value":{"nested":true}}}]}"#,
            "literal default must be a non-null scalar",
        ),
    ];

    for (name, raw, detail) in cases {
        let wire = serde_json::from_str::<pir::Program>(raw)
            .unwrap_or_else(|error| panic!("case {name} did not reach lowering: {error}"));
        let error = match super::lower_pir(wire) {
            Ok(_) => panic!("case {name} unexpectedly lowered"),
            Err(error) => error,
        };
        assert_eq!(
            error.reason(),
            ErrorReason::SchemaViolation,
            "case {name}: {error}"
        );
        assert!(error.to_string().contains(detail), "case {name}: {error}");
    }
}
