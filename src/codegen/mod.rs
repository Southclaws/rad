//! Schema-derived application client generation.
//!
//! This is deliberately separate from the LIR/PIR, HTTP, and CLI generators.
//! Those feed a published language runtime SDK; this module consumes an
//! accepted database schema and emits application-specific types which import
//! that SDK rather than reproducing its wire contracts.

mod go;
mod model;
mod output;

use std::path::PathBuf;

pub use model::{Column, Model, Relation, ScalarKind, Table};
pub use output::publish;

use crate::engine::catalog::model::Schema;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    pub package: String,
    pub schema_source: Vec<u8>,
    pub schema_version: u64,
    pub schema_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: Vec<u8>,
}

pub trait LanguageGenerator {
    fn name(&self) -> &'static str;
    fn generate(&self, model: &Model, options: &Options) -> Result<Vec<GeneratedFile>>;
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn generate(language: &str, schema: &Schema, options: &Options) -> Result<Vec<GeneratedFile>> {
    let model = Model::from_schema(schema)?;
    match language {
        "go" => go::Generator.generate(&model, options),
        other => Err(format!("unsupported client language {other:?} (supported: go)").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::schema;

    fn fixture() -> Schema {
        schema::parse(
            "fixture.rad.schema.yaml",
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
        .canonical()
    }

    #[test]
    fn generator_dispatch_is_explicit() {
        let options = Options {
            package: "db".into(),
            schema_source: b"tables: []\n".to_vec(),
            schema_version: 4,
            schema_hash: "sha256:accepted".into(),
        };
        assert!(generate("go", &fixture(), &options).is_ok());
        assert!(
            generate("typescript", &fixture(), &options)
                .unwrap_err()
                .to_string()
                .contains("supported: go")
        );
    }
}
