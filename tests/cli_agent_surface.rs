use std::process::{Command, Output};

#[test]
fn cli_exposes_version_matched_agent_guidance_and_machine_contracts() {
    let binary = env!("CARGO_BIN_EXE_rad");

    let skill = run(binary, &["skills", "get", "rad"]);
    assert_success(&skill);
    let skill = String::from_utf8(skill.stdout).unwrap();
    assert!(skill.starts_with("---\nname: rad\n"), "{skill}");
    assert!(skill.contains("Never add `--accept-data-loss`"), "{skill}");

    let full = run(binary, &["skills", "get", "rad", "--full"]);
    assert_success(&full);
    let full = String::from_utf8(full.stdout).unwrap();
    assert!(full.contains("# references/schema.md"), "{full}");
    assert!(full.contains("# references/operations.md"), "{full}");
    assert!(full.contains("# references/go-client.md"), "{full}");

    let skill_json = run(
        binary,
        &["--output", "json", "skills", "get", "rad", "--full"],
    );
    assert_success(&skill_json);
    let skill_json: serde_json::Value = serde_json::from_slice(&skill_json.stdout).unwrap();
    assert!(skill_json["skill"].as_str().unwrap().starts_with("---\n"));
    assert!(skill_json["references"]["references/schema.md"].is_string());

    let cache = tempfile::tempdir().unwrap();
    let materialized = Command::new(binary)
        .env("RAD_SKILLS_DIR", cache.path())
        .args(["--output", "json", "skills", "path", "rad"])
        .output()
        .unwrap();
    assert_success(&materialized);
    let materialized: serde_json::Value = serde_json::from_slice(&materialized.stdout).unwrap();
    let path = std::path::Path::new(materialized["path"].as_str().unwrap());
    assert!(path.join("SKILL.md").is_file());
    assert!(path.join("references/operations.md").is_file());
    assert!(!path.join("agents").exists());

    let schema = run(binary, &["schema", "json-schema"]);
    assert_success(&schema);
    let schema: serde_json::Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["$id"], "https://www.radengine.dev/rad.schema.json");

    let spec = run(binary, &["spec", "--format", "json"]);
    assert_success(&spec);
    let spec: serde_json::Value = serde_json::from_slice(&spec.stdout).unwrap();
    assert!(
        spec["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "skills")
    );
}

#[test]
fn json_errors_are_single_structured_documents() {
    let binary = env!("CARGO_BIN_EXE_rad");
    let missing = tempfile::tempdir()
        .unwrap()
        .path()
        .join("missing.rad.schema.yaml");
    let output = Command::new(binary)
        .args([
            "validate",
            "--file",
            &missing.display().to_string(),
            "--output",
            "json",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("schema file not found")
    );
}

#[test]
fn unattended_init_is_json_and_adds_schema_editor_metadata() {
    let binary = env!("CARGO_BIN_EXE_rad");
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("project");
    let output = Command::new(binary)
        .args([
            "init",
            "--yes",
            "--non-interactive",
            "--output",
            "json",
            &project.display().to_string(),
        ])
        .output()
        .unwrap();

    assert_success(&output);
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["root"], project.display().to_string());
    let schema = std::fs::read_to_string(project.join("rad.schema.yaml")).unwrap();
    assert!(schema.starts_with(
        "# yaml-language-server: $schema=https://www.radengine.dev/rad.schema.json\n"
    ));
}

fn run(binary: &str, args: &[&str]) -> Output {
    Command::new(binary).args(args).output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
