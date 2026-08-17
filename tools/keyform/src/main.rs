//! CLI: `keyform --spec protocol/storage.keyform --root . [--check | --rebaseline]`
//!
//! Compiles the specification: loads every referenced storage schema,
//! verifies the permanent allocation registry, then writes the generated
//! artifacts under the root. `--check` compares the artifacts against what
//! is on disk and fails on drift. `--rebaseline` skips the registry
//! verification and rewrites the registry from the current specification;
//! it is for deliberate pre-freeze format changes and requires wiping
//! existing stores.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use keyform::model::ValueForm;

const ALLOCATIONS_PATH: &str = "protocol/storage.allocations";

/// Emitted Rust is normalised through rustfmt so generated files are stable
/// under the repository's formatting gate and `--check` compares like with
/// like.
fn rustfmt(content: &str) -> Result<String, String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("spawn rustfmt: {error}"))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(content.as_bytes())
        .map_err(|error| format!("feed rustfmt: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for rustfmt: {error}"))?;
    if !output.status.success() {
        return Err(format!("rustfmt failed with {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("rustfmt output: {error}"))
}

fn main() -> ExitCode {
    let mut spec_path = PathBuf::from("protocol/storage.keyform");
    let mut root = PathBuf::from(".");
    let mut check = false;
    let mut rebaseline = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--spec" => {
                let Some(value) = arguments.next() else {
                    eprintln!("--spec needs a path");
                    return ExitCode::FAILURE;
                };
                spec_path = PathBuf::from(value);
            }
            "--root" => {
                let Some(value) = arguments.next() else {
                    eprintln!("--root needs a path");
                    return ExitCode::FAILURE;
                };
                root = PathBuf::from(value);
            }
            "--check" => check = true,
            "--rebaseline" => rebaseline = true,
            other => {
                eprintln!("unknown argument {other:?}");
                eprintln!("usage: keyform [--spec <path>] [--root <dir>] [--check] [--rebaseline]");
                return ExitCode::FAILURE;
            }
        }
    }
    if check && rebaseline {
        eprintln!("--check and --rebaseline are mutually exclusive");
        return ExitCode::FAILURE;
    }

    let source = match std::fs::read_to_string(&spec_path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("read {}: {error}", spec_path.display());
            return ExitCode::FAILURE;
        }
    };
    let spec = match keyform::parse(&source) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("{}: {error}", spec_path.display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(errors) = keyform::validate(&spec) {
        for error in errors {
            eprintln!("{}: {error}", spec_path.display());
        }
        return ExitCode::FAILURE;
    }

    let mut schemas = BTreeMap::new();
    for keyspace in &spec.keyspaces {
        for space in &keyspace.spaces {
            let ValueForm::JsonSchema(reference) = &space.value else {
                continue;
            };
            let schema_path = root.join(format!("protocol/storage/{reference}.schema.yaml"));
            let text = match std::fs::read_to_string(&schema_path) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!(
                        "space {} references schema {reference}: read {}: {error}",
                        space.name,
                        schema_path.display()
                    );
                    return ExitCode::FAILURE;
                }
            };
            let document: serde_yaml::Value = match serde_yaml::from_str(&text) {
                Ok(document) => document,
                Err(error) => {
                    eprintln!("{}: {error}", schema_path.display());
                    return ExitCode::FAILURE;
                }
            };
            match keyform::schema::schema_shape(&document) {
                Ok(shape) => {
                    schemas.insert(reference.clone(), shape);
                }
                Err(error) => {
                    eprintln!("{}: {error}", schema_path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let allocations = keyform::allocations::from_spec(&spec, &schemas);
    let allocations_path = root.join(ALLOCATIONS_PATH);
    if rebaseline {
        println!("rebaselining the allocation registry: released-meaning checks skipped");
    } else {
        // A missing registry must never bypass the released-meaning checks:
        // --rebaseline is the one sanctioned way to reset the baseline.
        let existing = match std::fs::read_to_string(&allocations_path) {
            Ok(existing) => existing,
            Err(error) => {
                eprintln!("{}: {error}", allocations_path.display());
                eprintln!(
                    "the allocation registry is required; pass --rebaseline to create a new baseline deliberately"
                );
                return ExitCode::FAILURE;
            }
        };
        let existing = match keyform::allocations::parse(&existing) {
            Ok(existing) => existing,
            Err(error) => {
                eprintln!("{}: {error}", allocations_path.display());
                return ExitCode::FAILURE;
            }
        };
        let violations = keyform::allocations::verify_compatible(&existing, &allocations);
        if !violations.is_empty() {
            for violation in violations {
                eprintln!("allocation registry: {violation}");
            }
            eprintln!(
                "released durable meanings are immutable; introduce a new allocation instead"
            );
            return ExitCode::FAILURE;
        }
    }

    let mut artifacts = keyform::emit(&spec, &source);
    artifacts.push((
        ALLOCATIONS_PATH.to_owned(),
        keyform::allocations::render(&allocations),
    ));
    let mut drifted = Vec::new();
    for (relative, content) in &artifacts {
        let content = &if relative.ends_with(".rs") {
            match rustfmt(content) {
                Ok(formatted) => formatted,
                Err(error) => {
                    eprintln!("{relative}: {error}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            content.clone()
        };
        let path = root.join(relative);
        if check {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if existing != *content {
                drifted.push(relative.clone());
            }
            continue;
        }
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("create {}: {error}", parent.display());
            return ExitCode::FAILURE;
        }
        if let Err(error) = std::fs::write(&path, content) {
            eprintln!("write {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {relative}");
    }
    if check {
        if drifted.is_empty() {
            println!("storage format artifacts match the specification");
        } else {
            for relative in &drifted {
                eprintln!("drift: {relative} does not match the specification");
            }
            eprintln!("run `task generate:storage`");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
