use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use openapi_to_rust::analysis::{OperationResponseBody, RequestBodyContent};
use openapi_to_rust::{CodeGenerator, ConfigFile, SchemaAnalyzer, TypeMapper};

const QUERY_OPERATION: &str = "Query";
const QUERY_MEDIA_TYPE: &str = "application/vnd.rad.lir+json";
const RESULT_MEDIA_TYPE: &str = "application/json";

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse()?;
    let mut config = ConfigFile::load(&arguments.config)?.into_generator_config();
    let spec_path = config.spec_path.clone();
    let spec_source = fs::read_to_string(&spec_path)?;
    let spec =
        openapi_to_rust::spec_source::parse_spec(&spec_source, &spec_path.to_string_lossy())?;
    openapi_to_rust::spec_source::validate_oas_document(&spec)?;
    require_codec_markers(&spec)?;

    config.apply_spec_server_default(&spec);
    let mapper = TypeMapper::new(config.types.clone());
    let mut analyzer = SchemaAnalyzer::with_type_mapper(spec, mapper)?;
    let mut analysis = analyzer.analyze()?;
    apply_query_codecs(&mut analysis)?;

    let generator = CodeGenerator::new(config).with_source_provenance("api/openapi.yaml");
    let result = generator.generate_all(&mut analysis)?;
    let artifacts = generator.output_artifacts(&result);
    if arguments.check {
        check_artifacts(generator.config().output_dir.as_path(), &artifacts)?;
    } else {
        write_artifacts(generator.config().output_dir.as_path(), &artifacts)?;
    }
    Ok(())
}

struct Arguments {
    config: PathBuf,
    check: bool,
}

impl Arguments {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut config = None;
        let mut check = false;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--config" => config = arguments.next().map(PathBuf::from),
                "--check" => check = true,
                _ => return Err(format!("unknown argument: {argument}").into()),
            }
        }
        Ok(Self {
            config: config.ok_or("--config is required")?,
            check,
        })
    }
}

fn require_codec_markers(spec: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    require_marker(
        spec,
        "/components/requestBodies/Query/content/application~1vnd.rad.lir+json/x-rad-rust-codec",
        "lir-query",
    )?;
    require_marker(
        spec,
        "/components/responses/QueryOK/content/application~1json/x-rad-rust-codec",
        "lir-result",
    )
}

fn require_marker(
    spec: &serde_json::Value,
    pointer: &str,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let actual = spec.pointer(pointer).and_then(serde_json::Value::as_str);
    if actual == Some(expected) {
        return Ok(());
    }
    Err(format!("OpenAPI marker {pointer} must be {expected:?}").into())
}

fn apply_query_codecs(
    analysis: &mut openapi_to_rust::SchemaAnalysis,
) -> Result<(), Box<dyn Error>> {
    let operation = analysis
        .operations
        .get_mut(QUERY_OPERATION)
        .ok_or("OpenAPI operation Query is required")?;
    match operation.request_body.as_ref() {
        Some(RequestBodyContent::Json { media_type, .. }) if media_type == QUERY_MEDIA_TYPE => {}
        _ => {
            return Err(format!("OpenAPI operation Query must use {QUERY_MEDIA_TYPE}").into());
        }
    }

    // The public media type remains JSON. The binary analysis kind instructs
    // the Rust generator to pass the bounded body as bytes. The handler then
    // decodes the independent LIR schema without a generic JSON tree. Only a
    // media type with an explicit Rad codec marker uses this path.
    operation.request_body = Some(RequestBodyContent::Binary {
        media_type: QUERY_MEDIA_TYPE.to_string(),
    });

    let response = analysis
        .operation_responses
        .get_mut(QUERY_OPERATION)
        .and_then(|responses| responses.get_mut("200"))
        .ok_or("OpenAPI response Query 200 is required")?;
    match response.body.as_ref() {
        Some(OperationResponseBody::Json { media_type, .. }) if media_type == RESULT_MEDIA_TYPE => {
        }
        _ => {
            return Err(format!("OpenAPI response Query 200 must use {RESULT_MEDIA_TYPE}").into());
        }
    }

    // The response schema name selects the generated JSON fallback. It must
    // be empty when the response body uses the direct byte representation.
    response.schema_name = None;
    response.media_type = Some(RESULT_MEDIA_TYPE.to_string());
    response.body = Some(OperationResponseBody::Binary {
        media_type: RESULT_MEDIA_TYPE.to_string(),
        wildcard: false,
    });
    Ok(())
}

fn write_artifacts(
    output_dir: &Path,
    artifacts: &BTreeMap<PathBuf, String>,
) -> Result<(), Box<dyn Error>> {
    for (relative, content) in artifacts {
        let path = output_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
    }
    Ok(())
}

fn check_artifacts(
    output_dir: &Path,
    artifacts: &BTreeMap<PathBuf, String>,
) -> Result<(), Box<dyn Error>> {
    let mut stale = Vec::new();
    for (relative, expected) in artifacts {
        let path = output_dir.join(relative);
        match fs::read_to_string(&path) {
            Ok(actual) if actual == *expected => {}
            Ok(_) => stale.push(format!("changed: {}", relative.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                stale.push(format!("missing: {}", relative.display()));
            }
            Err(error) => return Err(error.into()),
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "generated output is stale:\n  {}\nRun generation again to update it.",
            stale.join("\n  ")
        )
        .into())
    }
}
