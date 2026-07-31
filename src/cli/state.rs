use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::model::{
    ColumnDef, DefaultFunction, DefaultValue, ForeignKeyDef, IndexDef, ScalarType, Schema, TableDef,
};
use crate::http::generated::types as wire;
use crate::process::Result;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Lock {
    pub format_version: u32,
    pub schema_version: u64,
    pub schema_hash: String,
    pub snapshot: String,
}

#[derive(Clone, Debug)]
pub(super) struct Accepted {
    pub lock: Lock,
    pub source: Vec<u8>,
}

pub(super) struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub(super) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(super) fn lock_path(&self) -> PathBuf {
        self.root.join("schema.lock.json")
    }

    pub(super) fn snapshot_path(&self, version: u64) -> PathBuf {
        self.root.join("changelog").join(snapshot_name(version))
    }

    pub(super) fn load(&self) -> Result<Accepted> {
        let lock_path = self.lock_path();
        let lock: Lock = serde_json::from_slice(&read_file(&lock_path)?)
            .map_err(|error| format!("{}: {error}", lock_path.display()))?;
        if lock.format_version != FORMAT_VERSION {
            return Err(format!(
                "{}: unsupported format_version {}",
                lock_path.display(),
                lock.format_version
            )
            .into());
        }
        let expected = format!("changelog/{}", snapshot_name(lock.schema_version));
        if lock.snapshot != expected {
            return Err(format!(
                "{}: invalid snapshot {:?}; expected {:?}",
                lock_path.display(),
                lock.snapshot,
                expected
            )
            .into());
        }
        let snapshot = safe_snapshot_path(&self.root, &lock.snapshot)?;
        let source = read_file(&snapshot)?;
        let hash = parse_snapshot(&snapshot, &source)?.hash()?;
        if hash != lock.schema_hash {
            return Err(divergence(&snapshot, &lock.schema_hash, &hash));
        }
        Ok(Accepted { lock, source })
    }

    pub(super) fn write_accepted(&self, state: wire::SchemaState) -> Result<Accepted> {
        let accepted = self.write_snapshot(state)?;
        self.write_lock(&accepted.lock)?;
        Ok(accepted)
    }

    pub(super) fn write_snapshot(&self, state: wire::SchemaState) -> Result<Accepted> {
        let version = u64::try_from(state.schema_version)
            .map_err(|_| format!("invalid server schema version {}", state.schema_version))?;
        let schema = canonical_schema(state.schema)?;
        let hash = schema.hash()?;
        if hash != state.schema_hash {
            return Err(divergence(
                Path::new("server accepted schema"),
                &state.schema_hash,
                &hash,
            ));
        }
        let mut source = crate::engine::catalog::schema::render(&schema)?;
        let snapshot = self.snapshot_path(version);
        if snapshot.exists() {
            let existing = read_file(&snapshot)?;
            let existing_hash = parse_snapshot(&snapshot, &existing)?.hash()?;
            if existing_hash != hash {
                return Err(divergence(&snapshot, &hash, &existing_hash));
            }
            source = existing;
        } else {
            if let Some(parent) = snapshot.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match atomic_create(&snapshot, &source) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = read_file(&snapshot)?;
                    let existing_hash = parse_snapshot(&snapshot, &existing)?.hash()?;
                    if existing_hash != hash {
                        return Err(divergence(&snapshot, &hash, &existing_hash));
                    }
                    source = existing;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Accepted {
            lock: Lock {
                format_version: FORMAT_VERSION,
                schema_version: version,
                schema_hash: hash,
                snapshot: format!("changelog/{}", snapshot_name(version)),
            },
            source,
        })
    }

    pub(super) fn write_lock(&self, lock: &Lock) -> Result {
        let mut source = serde_json::to_vec_pretty(lock)?;
        source.push(b'\n');
        atomic_write(&self.lock_path(), &source)
    }

    pub(super) fn write_desired(path: &Path, source: &[u8]) -> Result {
        parse_snapshot(path, source)?;
        atomic_write(path, source)
    }

    pub(super) fn backup_desired(&self, path: &Path, now: DateTime<Utc>) -> Result<PathBuf> {
        let source = read_file(path)?;
        let backup = self
            .root
            .join("backups")
            .join(format!("{}.rad.schema.yaml", now.format("%Y%m%dT%H%M%SZ")));
        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_create(&backup, &source)?;
        Ok(backup)
    }
}

pub(super) fn matches_accepted(path: &Path, source: &[u8], accepted: &Accepted) -> Result<bool> {
    Ok(parse_snapshot(path, source)?.hash()? == accepted.lock.schema_hash)
}

fn snapshot_name(version: u64) -> String {
    format!("{version:08}.rad.schema.yaml")
}

fn parse_snapshot(path: &Path, source: &[u8]) -> Result<Schema> {
    Ok(crate::engine::catalog::schema::parse(&path.display().to_string(), source)?.canonical())
}

fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|error| std::io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

fn canonical_schema(document: wire::SchemaDocument) -> Result<Schema> {
    let mut tables = Vec::with_capacity(document.tables.len());
    for table in document.tables {
        let id = schema_id(table.id, "table")?;
        let mut columns = Vec::with_capacity(table.columns.len());
        for column in table.columns {
            let scalar_type = match column.r#type.as_str() {
                "text" => ScalarType::Text,
                "int64" => ScalarType::Int64,
                "float64" => ScalarType::Float64,
                "bool" => ScalarType::Bool,
                value => return Err(format!("unsupported column type {value:?}").into()),
            };
            columns.push(ColumnDef {
                id: schema_id(column.id, "column")?,
                name: column.name.clone(),
                scalar_type,
                nullable: column.nullable.unwrap_or(false),
                format: column.format.unwrap_or_default(),
                default: decode_default(&column.name, scalar_type, column.default)?,
            });
        }
        tables.push(TableDef {
            id,
            name: table.name,
            columns,
            primary_key: table.primary_key,
            indexes: table
                .indexes
                .unwrap_or_default()
                .into_iter()
                .map(|index| IndexDef {
                    name: index.name,
                    columns: index.columns,
                    unique: index.unique.unwrap_or(false),
                })
                .collect(),
            foreign_keys: table
                .foreign_keys
                .unwrap_or_default()
                .into_iter()
                .map(|foreign_key| ForeignKeyDef {
                    name: foreign_key.name,
                    columns: foreign_key.columns,
                    ref_table: foreign_key.ref_table,
                    ref_columns: foreign_key.ref_columns,
                })
                .collect(),
        });
    }
    Ok(Schema::from_definitions(tables))
}

fn schema_id(value: Option<i64>, role: &str) -> Result<SchemaId> {
    let value = value.ok_or_else(|| format!("server {role} definition has no schema ID"))?;
    let value = u32::try_from(value)
        .map_err(|_| format!("server {role} schema ID {value} is outside the supported range"))?;
    SchemaId::new(value).map_err(Into::into)
}

fn decode_default(
    column: &str,
    scalar_type: ScalarType,
    default: Option<wire::ColumnDefault>,
) -> Result<Option<DefaultValue>> {
    let Some(default) = default else {
        return Ok(None);
    };
    if let Some(function) = default.func {
        if default.value.is_some() {
            return Err(format!("column {column:?} default sets both func and value").into());
        }
        let function = match function.as_str() {
            "uuid" => DefaultFunction::Uuid,
            "now_ms" => DefaultFunction::NowMs,
            value => return Err(format!("unknown default function {value:?}").into()),
        };
        return Ok(Some(DefaultValue {
            function: Some(function),
            ..DefaultValue::default()
        }));
    }
    let value = default
        .value
        .ok_or_else(|| format!("column {column:?} default has neither func nor value"))?;
    let mut output = DefaultValue::default();
    match scalar_type {
        ScalarType::Text => {
            output.text = value
                .as_str()
                .ok_or_else(|| format!("column {column:?} default expects text"))?
                .into();
        }
        ScalarType::Int64 => {
            output.int64 = value
                .as_i64()
                .ok_or_else(|| format!("column {column:?} default expects int64"))?;
        }
        ScalarType::Float64 => {
            output.float64 = value
                .as_f64()
                .ok_or_else(|| format!("column {column:?} default expects float64"))?;
        }
        ScalarType::Bool => {
            output.bool_value = value
                .as_bool()
                .ok_or_else(|| format!("column {column:?} default expects bool"))?;
        }
    }
    Ok(Some(output))
}

fn safe_snapshot_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(format!("snapshot path escapes {}: {relative:?}", root.display()).into());
    }
    Ok(root.join(path))
}

fn atomic_create(path: &Path, source: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(source)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

fn atomic_write(path: &Path, source: &[u8]) -> Result {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(source)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn divergence(
    path: &Path,
    expected: &str,
    actual: &str,
) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "schema history diverged at {}: expected {expected}, found {actual}",
        path.display()
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_names_the_expected_lock_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path());

        let error = store.load().unwrap_err();

        assert!(
            error.to_string().contains(
                &directory
                    .path()
                    .join("schema.lock.json")
                    .display()
                    .to_string()
            ),
            "{error}"
        );
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn invalid_lock_snapshot_names_the_field_and_expected_value() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path());
        std::fs::write(
            store.lock_path(),
            r#"{"format_version":1,"schema_version":0,"schema_hash":"sha256:none","snapshot":""}"#,
        )
        .unwrap();

        let error = store.load().unwrap_err().to_string();

        assert!(error.contains("invalid snapshot \"\""), "{error}");
        assert!(
            error.contains("expected \"changelog/00000000.rad.schema.yaml\""),
            "{error}"
        );
    }

    #[test]
    fn snapshot_paths_cannot_escape_state() {
        assert!(safe_snapshot_path(Path::new("state"), "changelog/one.yaml").is_ok());
        assert!(safe_snapshot_path(Path::new("state"), "../one.yaml").is_err());
        assert!(safe_snapshot_path(Path::new("state"), "/one.yaml").is_err());
    }

    #[test]
    fn accepted_state_round_trips_and_detects_tampering() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path());
        let schema = crate::engine::catalog::schema::parse(
            "fixture",
            b"tables:\n  - id: 1\n    name: users\n    columns:\n      - { id: 1, name: id, type: string, pk: true }\n",
        )
        .unwrap()
        .canonical();
        let state = wire::SchemaState {
            schema_version: 4,
            schema_hash: schema.hash().unwrap(),
            schema: wire::SchemaDocument {
                tables: vec![wire::TableDef {
                    columns: vec![wire::ColumnDef {
                        default: None,
                        format: None,
                        id: Some(1),
                        name: "id".into(),
                        nullable: None,
                        r#type: "text".into(),
                    }],
                    foreign_keys: None,
                    id: Some(1),
                    indexes: None,
                    name: "users".into(),
                    primary_key: vec!["id".into()],
                }],
            },
        };
        store.write_accepted(state).unwrap();
        assert_eq!(store.load().unwrap().lock.schema_version, 4);
        std::fs::write(store.snapshot_path(4), b"tables: []\n").unwrap();
        assert!(store.load().is_err());
    }
}
