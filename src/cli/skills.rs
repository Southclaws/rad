use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::generated::GlobalArgs;
use super::output;
use crate::process::Result;

const SKILL: &str = include_str!("../../skill-data/rad/SKILL.md");
const SCHEMA: &str = include_str!("../../skill-data/rad/references/schema.md");
const OPERATIONS: &str = include_str!("../../skill-data/rad/references/operations.md");
const GO_CLIENT: &str = include_str!("../../skill-data/rad/references/go-client.md");

#[derive(Serialize)]
struct SkillSummary {
    name: &'static str,
    description: &'static str,
}

pub(super) fn list(globals: &GlobalArgs) -> Result {
    let skill = SkillSummary {
        name: "rad",
        description: "Build and operate schema-first Rad projects safely.",
    };
    if output::is_json(globals) {
        output::print_json(&serde_json::json!({ "skills": [skill] }))
    } else {
        println!("  rad  Build and operate schema-first Rad projects safely.");
        Ok(())
    }
}

pub(super) fn get(globals: &GlobalArgs, full: bool) -> Result {
    if output::is_json(globals) {
        let references = full.then(|| BTreeMap::from(references()));
        output::print_json(&serde_json::json!({
            "name": "rad",
            "skill": SKILL,
            "references": references,
        }))
    } else {
        print!("{SKILL}");
        if full {
            for (path, source) in references() {
                println!("\n---\n\n# {path}\n\n{source}");
            }
        }
        Ok(())
    }
}

pub(super) fn path() -> Result<PathBuf> {
    let root = cache_root().join("rad");
    write_if_changed(&root.join("SKILL.md"), SKILL)?;
    for (path, source) in references() {
        write_if_changed(&root.join(path), source)?;
    }
    Ok(root)
}

fn references() -> [(&'static str, &'static str); 3] {
    [
        ("references/schema.md", SCHEMA),
        ("references/operations.md", OPERATIONS),
        ("references/go-client.md", GO_CLIENT),
    ]
}

fn cache_root() -> PathBuf {
    if let Some(path) = std::env::var_os("RAD_SKILLS_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("rad/skills");
    }
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("rad/skills");
    }
    if let Some(path) = std::env::var_os("HOME") {
        return PathBuf::from(path).join(".cache/rad/skills");
    }
    std::env::temp_dir().join("rad/skills")
}

fn write_if_changed(path: &Path, source: &str) -> Result {
    if std::fs::read(path).is_ok_and(|current| current == source.as_bytes()) {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, source.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
