use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde::Serialize;

use super::{GeneratedFile, LanguageGenerator, Model, Options, Result, ScalarKind};

pub struct Generator;

const CLIENT_TEMPLATE: &str = include_str!("templates/client.go.j2");
const RUNTIME_TEMPLATE: &str = include_str!("templates/runtime.go.j2");
const RECORD_TEMPLATE: &str = include_str!("templates/record.go.j2");
const TABLE_TEMPLATE: &str = include_str!("templates/table.go.j2");
const MACROS_TEMPLATE: &str = include_str!("templates/macros.go.j2");

impl LanguageGenerator for Generator {
    fn name(&self) -> &'static str {
        "go"
    }

    fn generate(&self, model: &Model, options: &Options) -> Result<Vec<GeneratedFile>> {
        let view = View::new(model, options)?;
        let mut environment = Environment::new();
        environment.set_undefined_behavior(UndefinedBehavior::Strict);
        environment.set_auto_escape_callback(|_| AutoEscape::None);
        environment.set_trim_blocks(true);
        environment.set_lstrip_blocks(true);
        environment.set_keep_trailing_newline(true);
        environment.add_template("client", CLIENT_TEMPLATE)?;
        environment.add_template("runtime", RUNTIME_TEMPLATE)?;
        environment.add_template("record", RECORD_TEMPLATE)?;
        environment.add_template("table", TABLE_TEMPLATE)?;
        environment.add_template("macros", MACROS_TEMPLATE)?;
        let mut content = environment.get_template("client")?.render(&view)?;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        Ok(vec![GeneratedFile {
            path: "rad_client_gen.go".into(),
            content: format_source(&content)?,
        }])
    }
}

fn format_source(source: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("gofmt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "cannot format generated Go source: failed to start gofmt: {error}; install the Go toolchain"
            )
        })?;
    child
        .stdin
        .take()
        .expect("piped stdin exists")
        .write_all(source.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "gofmt rejected generated Go source:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output.stdout)
}

#[derive(Serialize)]
struct View {
    package: String,
    schema_version: u64,
    schema_hash_quoted: String,
    schema_source_quoted: String,
    tables: Vec<TableView>,
}

#[derive(Clone, Serialize)]
struct TableView {
    name: String,
    name_quoted: String,
    model: String,
    lower_model: String,
    handle_field: String,
    query: String,
    include: String,
    columns: Vec<ColumnView>,
    forward_relations: Vec<RelationView>,
    reverse_relations: Vec<RelationView>,
    unique_indexes: Vec<UniqueView>,
    pk_params: String,
    pk_key_map: String,
    default_order_terms: String,
}

#[derive(Clone, Serialize)]
struct ColumnView {
    name: String,
    name_quoted: String,
    field: String,
    lower_field: String,
    go_type: String,
    nullable: bool,
    has_default: bool,
    primary_key: bool,
    ordered: bool,
    numeric: bool,
    json_suffix: String,
    record_helper: String,
    record_pointer_helper: String,
    key_type: String,
}

#[derive(Clone, Serialize)]
struct RelationView {
    field: String,
    as_name: String,
    as_quoted: String,
    target_name_quoted: String,
    target_model: String,
    target_lower_model: String,
    pairs_literal: String,
}

#[derive(Clone, Serialize)]
struct UniqueView {
    method: String,
    columns_doc: String,
    params: String,
    columns: Vec<ColumnView>,
}

impl View {
    fn new(model: &Model, options: &Options) -> Result<Self> {
        validate_identifier("Go package", &options.package, false)?;
        if is_keyword(&options.package) {
            return Err(format!("Go package name {:?} is reserved", options.package).into());
        }

        let model_names = model
            .tables
            .iter()
            .map(|table| (table.name.as_str(), model_name(&table.name)))
            .collect::<HashMap<_, _>>();
        let mut seen_models = HashSet::new();
        for (table, name) in &model_names {
            if !seen_models.insert(name.clone()) {
                return Err(format!(
                    "Go model name collision: table {table:?} also maps to {name:?}"
                )
                .into());
            }
        }

        let relation_counts = model
            .tables
            .iter()
            .flat_map(|table| &table.forward_relations)
            .fold(
                HashMap::<(&str, &str), usize>::new(),
                |mut counts, relation| {
                    *counts
                        .entry((&relation.source_table, &relation.target_table))
                        .or_default() += 1;
                    counts
                },
            );

        let mut tables = Vec::with_capacity(model.tables.len());
        for table in &model.tables {
            let model_name = model_names[table.name.as_str()].clone();
            let columns = table
                .columns
                .iter()
                .map(ColumnView::new)
                .collect::<Vec<_>>();
            ensure_unique_fields(&table.name, &columns)?;
            let columns_by_name = columns
                .iter()
                .map(|column| (column.name.as_str(), column.clone()))
                .collect::<HashMap<_, _>>();

            let forward_relations = table
                .forward_relations
                .iter()
                .map(|relation| {
                    let field = forward_name(&relation.source_columns[0]);
                    RelationView::new(
                        field,
                        &relation.target_table,
                        &model_names[relation.target_table.as_str()],
                        &relation.pairs,
                    )
                })
                .collect::<Vec<_>>();
            let reverse_relations = table
                .reverse_relations
                .iter()
                .map(|relation| {
                    let mut field = go_exported(&relation.source_table);
                    if relation_counts[&(
                        relation.source_table.as_str(),
                        relation.target_table.as_str(),
                    )] > 1
                    {
                        field.push_str("By");
                        field.push_str(&forward_name(&relation.source_columns[0]));
                    }
                    RelationView::new(
                        field,
                        &relation.source_table,
                        &model_names[relation.source_table.as_str()],
                        &relation.pairs,
                    )
                })
                .collect::<Vec<_>>();
            ensure_relation_fields(
                &table.name,
                &columns,
                forward_relations.iter().chain(&reverse_relations),
            )?;

            let primary_key = table
                .primary_key
                .iter()
                .map(|name| {
                    columns_by_name.get(name.as_str()).cloned().ok_or_else(|| {
                        format!(
                            "table {:?} has unknown primary-key column {name:?}",
                            table.name
                        )
                    })
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let unique_indexes = table
                .unique_indexes
                .iter()
                .map(|index| {
                    let index_columns = index
                        .iter()
                        .map(|name| {
                            columns_by_name.get(name.as_str()).cloned().ok_or_else(|| {
                                format!(
                                    "table {:?} has unknown unique-index column {name:?}",
                                    table.name
                                )
                            })
                        })
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(UniqueView::new(index_columns))
                })
                .collect::<std::result::Result<Vec<_>, String>>()?;

            tables.push(TableView {
                name: table.name.clone(),
                name_quoted: quote(&table.name),
                lower_model: lower_first(&model_name),
                handle_field: go_exported(&table.name),
                query: format!("{model_name}Query"),
                include: format!("{model_name}Include"),
                pk_params: parameters(&primary_key),
                pk_key_map: key_map(&primary_key),
                default_order_terms: order_terms(&primary_key),
                model: model_name,
                columns,
                forward_relations,
                reverse_relations,
                unique_indexes,
            });
        }

        Ok(Self {
            package: options.package.clone(),
            schema_version: options.schema_version,
            schema_hash_quoted: quote(&options.schema_hash),
            schema_source_quoted: quote(std::str::from_utf8(&options.schema_source)?),
            tables,
        })
    }
}

impl ColumnView {
    fn new(column: &super::Column) -> Self {
        let go_type = match column.kind {
            ScalarKind::Text => "string",
            ScalarKind::Int64 => "int64",
            ScalarKind::Float64 => "float64",
            ScalarKind::Bool => "bool",
        };
        let helper = match column.kind {
            ScalarKind::Text => "String",
            ScalarKind::Int64 => "Int64",
            ScalarKind::Float64 => "Float64",
            ScalarKind::Bool => "Bool",
        };
        let field = go_exported(&column.name);
        Self {
            name: column.name.clone(),
            name_quoted: quote(&column.name),
            lower_field: lower_first(&field),
            field,
            go_type: go_type.into(),
            nullable: column.nullable,
            has_default: column.has_default,
            primary_key: column.primary_key,
            ordered: !matches!(column.kind, ScalarKind::Bool),
            numeric: matches!(column.kind, ScalarKind::Int64 | ScalarKind::Float64),
            json_suffix: if column.nullable { ",omitempty" } else { "" }.into(),
            record_helper: format!("rec{helper}{}", if column.nullable { "Ptr" } else { "" }),
            record_pointer_helper: format!("rec{helper}Ptr"),
            key_type: format!("{}{go_type}", if column.nullable { "*" } else { "" }),
        }
    }
}

impl RelationView {
    fn new(
        field: String,
        target_name: &str,
        target_model: &str,
        pairs: &[(String, String)],
    ) -> Self {
        Self {
            as_name: snake(&field),
            as_quoted: quote(&snake(&field)),
            target_name_quoted: quote(target_name),
            target_lower_model: lower_first(target_model),
            target_model: target_model.into(),
            pairs_literal: pairs_literal(pairs),
            field,
        }
    }
}

impl UniqueView {
    fn new(columns: Vec<ColumnView>) -> Self {
        Self {
            method: format!(
                "By{}",
                columns
                    .iter()
                    .map(|column| column.field.as_str())
                    .collect::<String>()
            ),
            columns_doc: columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            params: parameters(&columns),
            columns,
        }
    }
}

fn ensure_unique_fields(table: &str, columns: &[ColumnView]) -> Result<()> {
    let mut seen = HashSet::new();
    for column in columns {
        validate_identifier("generated Go field", &column.field, true)?;
        if !seen.insert(&column.field) {
            return Err(format!(
                "Go field collision in table {table:?}: multiple columns map to {:?}",
                column.field
            )
            .into());
        }
    }
    Ok(())
}

fn ensure_relation_fields<'a>(
    table: &str,
    columns: &[ColumnView],
    relations: impl Iterator<Item = &'a RelationView>,
) -> Result<()> {
    let mut seen = columns
        .iter()
        .map(|column| column.field.as_str())
        .collect::<HashSet<_>>();
    for relation in relations {
        if !seen.insert(&relation.field) {
            return Err(format!(
                "Go field collision in table {table:?}: relation maps to {:?}",
                relation.field
            )
            .into());
        }
    }
    Ok(())
}

fn model_name(table: &str) -> String {
    let mut parts = table.split('_').map(str::to_owned).collect::<Vec<_>>();
    if let Some(last) = parts.last_mut() {
        if let Some(stem) = last.strip_suffix("ies") {
            *last = format!("{stem}y");
        } else if ["sses", "xes", "zes", "ches", "shes"]
            .iter()
            .any(|suffix| last.ends_with(suffix))
        {
            last.truncate(last.len() - 2);
        } else if last.ends_with('s') && !last.ends_with("ss") && last.len() > 1 {
            last.pop();
        }
    }
    go_exported(&parts.join("_"))
}

fn forward_name(column: &str) -> String {
    column
        .strip_suffix("_id")
        .filter(|name| !name.is_empty())
        .map(go_exported)
        .unwrap_or_else(|| format!("{}Ref", go_exported(column)))
}

fn go_exported(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part == "id" {
                "ID".into()
            } else {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect()
}

fn lower_first(value: &str) -> String {
    if value == "ID" {
        return "id".into();
    }
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_lowercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn snake(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_uppercase() {
            if index > 0 {
                let previous_lower = bytes[index - 1].is_ascii_lowercase();
                let next_lower = bytes.get(index + 1).is_some_and(u8::is_ascii_lowercase);
                if previous_lower || next_lower {
                    output.push('_');
                }
            }
            output.push(byte.to_ascii_lowercase() as char);
        } else {
            output.push(byte as char);
        }
    }
    output
}

fn parameters(columns: &[ColumnView]) -> String {
    columns
        .iter()
        .map(|column| format!("{} {}", column.lower_field, column.go_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn key_map(columns: &[ColumnView]) -> String {
    format!(
        "map[string]any{{{}}}",
        columns
            .iter()
            .map(|column| format!("{}: {}", column.name_quoted, column.lower_field))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn order_terms(columns: &[ColumnView]) -> String {
    format!(
        "[]lirwire.OrderTerm{{{}}}",
        columns
            .iter()
            .map(|column| format!("{{Expr: lirwire.Col(\"\", {})}}", column.name_quoted))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn pairs_literal(pairs: &[(String, String)]) -> String {
    format!(
        "[][2]string{{{}}}",
        pairs
            .iter()
            .map(|(left, right)| format!("{{{}, {}}}", quote(left), quote(right)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("a Rust string always encodes as JSON")
}

fn validate_identifier(role: &str, value: &str, exported: bool) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(format!("{role} must not be empty").into());
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || chars.any(|character| !(character == '_' || character.is_ascii_alphanumeric()))
        || (exported && !first.is_ascii_uppercase())
    {
        return Err(format!("invalid {role} {value:?}").into());
    }
    Ok(())
}

fn is_keyword(value: &str) -> bool {
    matches!(
        value,
        "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::schema;

    #[test]
    fn emits_embedded_templates_with_schema_identity_and_relations() {
        let schema = schema::parse(
            "fixture",
            br#"
tables:
  - id: 1
    name: categories
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: name, type: string, unique: true }
      - { id: 3, name: parent_id, type: string, nullable: true }
      - { id: 4, name: rank, type: int64, default: 0 }
    foreign_keys:
      - name: categories_parent_id_fk
        columns: [parent_id]
        ref_table: categories
        ref_columns: [id]
"#,
        )
        .unwrap()
        .canonical();
        let files = Generator
            .generate(
                &Model::from_schema(&schema).unwrap(),
                &Options {
                    package: "testclient".into(),
                    schema_source: b"tables: []\n".to_vec(),
                    schema_version: 4,
                    schema_hash: "sha256:accepted".into(),
                },
            )
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, std::path::Path::new("rad_client_gen.go"));
        let source = std::str::from_utf8(&files[0].content).unwrap();
        for expected in [
            "package testclient",
            "const SchemaVersion uint64 = 4",
            "const SchemaHash = \"sha256:accepted\"",
            "const RawSchema = \"tables: []\\n\"",
            "type Category struct",
            "IncludeParent",
            "IncludeCategories",
            "lirwire.",
            "github.com/Southclaws/rad/clients/go/rad",
            "github.com/Southclaws/rad/clients/go/protocol",
        ] {
            assert!(
                source.contains(expected),
                "missing {expected:?} in generated source:\n{source}"
            );
        }
        for forbidden in [
            "protocol.Eq",
            "protocol.Col(",
            "protocol.Node",
            "protocol.OrderTerm",
            "protocol.AggTerm",
            "protocol.Field",
            "protocol.Query",
        ] {
            assert!(
                !source.contains(forbidden),
                "generated source must consume the runtime SDK, not recreate {forbidden:?}:\n{source}"
            );
        }
    }

    #[test]
    fn rejects_go_identifier_collisions_and_reserved_packages() {
        let schema = schema::parse(
            "fixture",
            br#"
tables:
  - id: 1
    name: widgets
    columns:
      - { id: 1, name: item_id, type: string, pk: true }
      - { id: 2, name: item__id, type: string }
"#,
        )
        .unwrap()
        .canonical();
        let model = Model::from_schema(&schema).unwrap();
        let options = Options {
            package: "client".into(),
            schema_source: b"tables: []\n".to_vec(),
            schema_version: 1,
            schema_hash: "sha256:accepted".into(),
        };
        assert!(Generator.generate(&model, &options).is_err());

        let mut reserved = options;
        reserved.package = "type".into();
        assert!(
            Generator
                .generate(&Model { tables: vec![] }, &reserved)
                .is_err()
        );
    }
}
