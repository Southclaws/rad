use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{GeneratedFile, Result};

const MANIFEST_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct Manifest {
    format_version: u32,
    generator: String,
    files: Vec<PathBuf>,
}

pub fn publish(root: &Path, generator: &str, files: Vec<GeneratedFile>) -> Result<Vec<PathBuf>> {
    validate_generator_name(generator)?;
    let mut seen = HashSet::with_capacity(files.len());
    for file in &files {
        validate_relative(&file.path)?;
        if !seen.insert(file.path.clone()) {
            return Err(format!("generator {generator:?} emitted {:?} twice", file.path).into());
        }
    }
    std::fs::create_dir_all(root)?;

    let manifest_path = root.join(format!(".rad-codegen-{generator}.json"));
    let previous = load_manifest(&manifest_path, generator)?;
    for file in &files {
        atomic_write(&root.join(&file.path), &file.content)?;
    }

    let current = files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    for path in previous.files {
        validate_relative(&path)?;
        if !seen.contains(&path) {
            match std::fs::remove_file(root.join(path)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    let manifest = Manifest {
        format_version: MANIFEST_VERSION,
        generator: generator.into(),
        files: current.clone(),
    };
    let mut source = serde_json::to_vec_pretty(&manifest)?;
    source.push(b'\n');
    atomic_write(&manifest_path, &source)?;
    Ok(current)
}

fn load_manifest(path: &Path, generator: &str) -> Result<Manifest> {
    let source = match std::fs::read(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Manifest {
                format_version: MANIFEST_VERSION,
                generator: generator.into(),
                files: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: Manifest = serde_json::from_slice(&source)
        .map_err(|error| format!("{}: invalid codegen manifest: {error}", path.display()))?;
    if manifest.format_version != MANIFEST_VERSION || manifest.generator != generator {
        return Err(format!("{}: incompatible codegen manifest", path.display()).into());
    }
    Ok(manifest)
}

fn validate_generator_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("unsafe generator name {name:?}").into());
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("generator emitted unsafe relative path {:?}", path).into());
    }
    Ok(())
}

fn atomic_write(path: &Path, source: &[u8]) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_and_duplicate_generator_paths() {
        let directory = tempfile::tempdir().unwrap();
        let escaping = vec![GeneratedFile {
            path: "../outside.go".into(),
            content: Vec::new(),
        }];
        assert!(publish(directory.path(), "go", escaping).is_err());
        let duplicate = vec![
            GeneratedFile {
                path: "client.go".into(),
                content: b"one".to_vec(),
            },
            GeneratedFile {
                path: "client.go".into(),
                content: b"two".to_vec(),
            },
        ];
        assert!(publish(directory.path(), "go", duplicate).is_err());
    }

    #[test]
    fn removes_only_files_owned_by_the_same_generator() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("handwritten.go"), "package db\n").unwrap();
        publish(
            directory.path(),
            "go",
            vec![GeneratedFile {
                path: "first.go".into(),
                content: b"first".to_vec(),
            }],
        )
        .unwrap();
        publish(
            directory.path(),
            "go",
            vec![GeneratedFile {
                path: "second.go".into(),
                content: b"second".to_vec(),
            }],
        )
        .unwrap();
        assert!(!directory.path().join("first.go").exists());
        assert!(directory.path().join("second.go").exists());
        assert!(directory.path().join("handwritten.go").exists());
    }
}
