//! Offline request-schema validators and normalized public rejections.
#![allow(dead_code)]
use super::errors::{InvalidParameter, ProblemDetails, RequestValidationRejection};
const VALIDATION_SCHEMA: &str = "{\"$schema\":\"http://json-schema.org/draft-04/schema#\",\"definitions\":{\"components\":{\"component_ColumnDef\":{\"description\":\"A column definition for a direct catalog create operation.\",\"properties\":{\"default\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_ColumnDefault\"},\"format\":{\"description\":\"An optional semantic hint such as `uuid` or `unix_ms`.\",\"type\":\"string\"},\"id\":{\"description\":\"An optional stable logical identity; direct mode allocates one when omitted.\",\"format\":\"int64\",\"maximum\":2147483647,\"minimum\":1,\"type\":\"integer\"},\"name\":{\"type\":\"string\"},\"nullable\":{\"type\":\"boolean\"},\"type\":{\"description\":\"The column's storage type, one of `text`, `int64`, `float64`, or `bool`.\",\"type\":\"string\"}},\"required\":[\"name\",\"type\"],\"type\":\"object\"},\"component_ColumnDefault\":{\"description\":\"A column default, applied when an insert omits the column: either a\\nbuiltin generator named by `func` (`uuid` on text columns, `now_ms`\\non int64 columns) or a literal `value` of the column's type. Exactly\\none of the two is set.\\n\",\"properties\":{\"func\":{\"description\":\"A builtin generator, `uuid` or `now_ms`.\",\"type\":\"string\"},\"value\":{\"allOf\":[{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_Value\"}],\"description\":\"A literal of the column's type.\"}},\"type\":\"object\"},\"component_ColumnUpdateProps\":{\"description\":\"The column properties to update.\",\"properties\":{\"name\":{\"description\":\"The column's new name.\",\"type\":\"string\"}},\"required\":[\"name\"],\"type\":\"object\"},\"component_ForeignKeyInfo\":{\"description\":\"One foreign key, in both definitions and introspection. The\\nreferenced columns must be the referenced table's full primary key.\\n\",\"properties\":{\"columns\":{\"description\":\"The referencing column names on this table.\",\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"ref_columns\":{\"description\":\"The referenced table's primary key columns, in order.\",\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"ref_table\":{\"description\":\"The referenced table's name.\",\"type\":\"string\"}},\"required\":[\"name\",\"columns\",\"ref_table\",\"ref_columns\"],\"type\":\"object\"},\"component_IndexInfo\":{\"description\":\"One secondary index, in both definitions and introspection.\",\"properties\":{\"columns\":{\"description\":\"The indexed column names, in order.\",\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"unique\":{\"type\":\"boolean\"}},\"required\":[\"name\",\"columns\"],\"type\":\"object\"},\"component_Program\":{\"description\":\"An arbitrary JSON object containing a PIR execution program. As with\\n`Query`, the HTTP contract does not describe the PIR grammar; servers\\nvalidate this raw body against the independent PIR JSON Schema, and\\neach statement's relation against the LIR schema.\\n\"},\"component_SchemaCompatibilityRequest\":{\"additionalProperties\":false,\"properties\":{\"schema_hash\":{\"type\":\"string\"},\"schema_version\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"}},\"required\":[\"schema_version\",\"schema_hash\"],\"type\":\"object\"},\"component_SchemaMigrateRequest\":{\"additionalProperties\":false,\"description\":\"A desired schema, the preflighted server identity, and explicit data-loss consent.\",\"properties\":{\"accept_data_loss\":{\"type\":\"boolean\"},\"current_hash\":{\"type\":\"string\"},\"current_version\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"},\"schema\":{\"type\":\"string\"}},\"required\":[\"schema\",\"current_version\",\"current_hash\"],\"type\":\"object\"},\"component_SchemaRequest\":{\"additionalProperties\":false,\"description\":\"A desired schema source to plan.\",\"properties\":{\"schema\":{\"description\":\"The full `rad.schema.yaml` source document, as YAML.\",\"type\":\"string\"}},\"required\":[\"schema\"],\"type\":\"object\"},\"component_TableDef\":{\"description\":\"A new table's definition, mirroring a `rad.schema.yaml` entry as JSON. The\\ndirect API may omit logical IDs for the catalog to allocate. Column\\ntypes are `text`, `int64`, `float64`, or `bool`; the primary key is\\nrequired and its columns must not be nullable.\\n\",\"properties\":{\"columns\":{\"items\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_ColumnDef\"},\"type\":\"array\"},\"foreign_keys\":{\"items\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_ForeignKeyInfo\"},\"type\":\"array\"},\"id\":{\"description\":\"An optional stable logical identity; direct mode allocates one when omitted.\",\"format\":\"int64\",\"maximum\":2147483647,\"minimum\":1,\"type\":\"integer\"},\"indexes\":{\"items\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_IndexInfo\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"primary_key\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"}},\"required\":[\"name\",\"columns\",\"primary_key\"],\"type\":\"object\"},\"component_TableUpdateProps\":{\"description\":\"The table properties to update.\",\"properties\":{\"name\":{\"description\":\"The table's new name.\",\"type\":\"string\"}},\"required\":[\"name\"],\"type\":\"object\"},\"component_TransitionKind\":{\"description\":\"The physical protocol used to perform online schema work.\",\"enum\":[\"index_build\",\"column_replacement\",\"constraint_validation\"],\"type\":\"string\"},\"component_TransitionState\":{\"description\":\"The durable lifecycle state of online schema work.\",\"enum\":[\"waiting\",\"building\",\"catching_up\",\"validating\",\"ready\",\"failed\",\"cancelled\"],\"type\":\"string\"},\"component_Value\":{\"description\":\"An arbitrary JSON value carried by the HTTP protocol.\"}},\"targets\":{\"v0\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_SchemaRequest\"},\"v1\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_SchemaMigrateRequest\"},\"v10\":{\"type\":\"string\"},\"v11\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_ColumnDef\"},\"v12\":{\"type\":\"string\"},\"v13\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_ColumnUpdateProps\"},\"v14\":{\"type\":\"string\"},\"v15\":{\"type\":\"string\"},\"v16\":{\"type\":\"string\"},\"v17\":{\"type\":\"string\"},\"v18\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_IndexInfo\"},\"v19\":{\"type\":\"string\"},\"v2\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_SchemaCompatibilityRequest\"},\"v20\":{\"type\":\"string\"},\"v21\":{\"type\":\"string\"},\"v22\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_Program\"},\"v23\":{\"type\":\"boolean\"},\"v24\":{\"type\":\"boolean\"},\"v3\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_TransitionKind\"},\"v4\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_TransitionState\"},\"v5\":{\"minLength\":1,\"type\":\"string\"},\"v6\":{\"minLength\":1,\"type\":\"string\"},\"v7\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_TableDef\"},\"v8\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/definitions/components/component_TableUpdateProps\"},\"v9\":{\"type\":\"string\"}}},\"id\":\"urn:openapi-to-rust:request-validation\"}";
const MAX_VALIDATION_ERRORS: usize = 16usize;
pub(crate) const VALIDATION_TARGET_0_BODY: &str = "#/definitions/targets/v0";
pub(crate) const VALIDATION_TARGET_1_BODY: &str = "#/definitions/targets/v1";
pub(crate) const VALIDATION_TARGET_2_BODY: &str = "#/definitions/targets/v2";
pub(crate) const VALIDATION_TARGET_3_QUERY_0: &str = "#/definitions/targets/v3";
pub(crate) const VALIDATION_TARGET_4_QUERY_1: &str = "#/definitions/targets/v4";
pub(crate) const VALIDATION_TARGET_5_PATH_0: &str = "#/definitions/targets/v5";
pub(crate) const VALIDATION_TARGET_6_PATH_0: &str = "#/definitions/targets/v6";
pub(crate) const VALIDATION_TARGET_7_BODY: &str = "#/definitions/targets/v7";
pub(crate) const VALIDATION_TARGET_8_BODY: &str = "#/definitions/targets/v8";
pub(crate) const VALIDATION_TARGET_9_PATH_0: &str = "#/definitions/targets/v9";
pub(crate) const VALIDATION_TARGET_10_PATH_0: &str = "#/definitions/targets/v10";
pub(crate) const VALIDATION_TARGET_11_BODY: &str = "#/definitions/targets/v11";
pub(crate) const VALIDATION_TARGET_12_PATH_0: &str = "#/definitions/targets/v12";
pub(crate) const VALIDATION_TARGET_13_BODY: &str = "#/definitions/targets/v13";
pub(crate) const VALIDATION_TARGET_14_PATH_0: &str = "#/definitions/targets/v14";
pub(crate) const VALIDATION_TARGET_15_PATH_1: &str = "#/definitions/targets/v15";
pub(crate) const VALIDATION_TARGET_16_PATH_0: &str = "#/definitions/targets/v16";
pub(crate) const VALIDATION_TARGET_17_PATH_1: &str = "#/definitions/targets/v17";
pub(crate) const VALIDATION_TARGET_18_BODY: &str = "#/definitions/targets/v18";
pub(crate) const VALIDATION_TARGET_19_PATH_0: &str = "#/definitions/targets/v19";
pub(crate) const VALIDATION_TARGET_20_PATH_0: &str = "#/definitions/targets/v20";
pub(crate) const VALIDATION_TARGET_21_PATH_1: &str = "#/definitions/targets/v21";
pub(crate) const VALIDATION_TARGET_22_BODY: &str = "#/definitions/targets/v22";
pub(crate) const VALIDATION_TARGET_23_QUERY_0: &str = "#/definitions/targets/v23";
pub(crate) const VALIDATION_TARGET_24_QUERY_1: &str = "#/definitions/targets/v24";
static VALIDATORS: ::std::sync::LazyLock<
    ::std::result::Result<::jsonschema::ValidatorMap, ()>,
> = ::std::sync::LazyLock::new(|| {
    let schema: ::serde_json::Value = ::serde_json::from_str(VALIDATION_SCHEMA)
        .map_err(|_| ())?;
    ::jsonschema::options()
        .with_draft(::jsonschema::Draft::Draft4)
        .should_validate_formats(true)
        .with_pattern_options(::jsonschema::PatternOptions::regex())
        .build_map(&schema)
        .map_err(|_| ())
});
pub(crate) async fn decode_json_body<T>(
    request: ::axum::extract::Request,
    target: &str,
    expected_media_type: &str,
    required: bool,
    max_body_bytes: usize,
) -> ::std::result::Result<::std::option::Option<T>, RequestValidationRejection>
where
    T: ::serde::de::DeserializeOwned,
{
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(::axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let is_json = content_type
        .is_some_and(|value| media_type_is(value, expected_media_type));
    if !is_json {
        if content_type.is_none() && !required {
            let bytes = read_body(body, max_body_bytes).await?;
            if bytes.is_empty() {
                return Ok(None);
            }
        }
        return Err(unsupported_media_type());
    }
    let bytes = read_body(body, max_body_bytes).await?;
    if bytes.is_empty() {
        return if required { Err(malformed_request()) } else { Ok(None) };
    }
    let instance: ::serde_json::Value = ::serde_json::from_slice(&bytes)
        .map_err(|_| malformed_request())?;
    validate(target, "/body", &instance)?;
    let typed = ::serde_json::from_value(instance)
        .map_err(|_| generated_contract_error())?;
    Ok(Some(typed))
}
pub(crate) fn decode_parameter<T>(
    raw: &str,
    target: &str,
    location: &str,
    string_wire: bool,
) -> ::std::result::Result<T, RequestValidationRejection>
where
    T: ::serde::de::DeserializeOwned + ::serde::Serialize,
{
    let typed = if string_wire {
        let instance = ::serde_json::Value::String(raw.to_string());
        validate(target, location, &instance)?;
        ::serde_json::from_value(instance).map_err(|_| generated_contract_error())?
    } else {
        ::serde_json::from_value(::serde_json::Value::String(raw.to_string()))
            .or_else(|_| ::serde_json::from_str(raw))
            .map_err(|_| malformed_parameter(location))?
    };
    validate_parameter(target, location, &typed)?;
    Ok(typed)
}
pub(crate) fn validate_string_parameter(
    target: &str,
    location: &str,
    raw: &str,
) -> ::std::result::Result<(), RequestValidationRejection> {
    validate(target, location, &::serde_json::Value::String(raw.to_string()))
}
pub(crate) fn validate_parameter<T>(
    target: &str,
    location: &str,
    value: &T,
) -> ::std::result::Result<(), RequestValidationRejection>
where
    T: ::serde::Serialize + ?Sized,
{
    let instance = ::serde_json::to_value(value)
        .map_err(|_| generated_contract_error())?;
    validate(target, location, &instance)
}
pub(crate) fn parse_cookies(
    headers: &::axum::http::HeaderMap,
) -> ::std::result::Result<
    ::std::collections::BTreeMap<String, String>,
    RequestValidationRejection,
> {
    let mut cookies = ::std::collections::BTreeMap::new();
    for header in headers.get_all(::axum::http::header::COOKIE).iter() {
        let line = header.to_str().map_err(|_| malformed_parameter("/cookie"))?;
        for field in line.split(';') {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| malformed_parameter("/cookie"))?;
            let name = name.trim();
            if name.is_empty()
                || cookies.insert(name.to_string(), value.to_string()).is_some()
            {
                return Err(malformed_parameter("/cookie"));
            }
        }
    }
    Ok(cookies)
}
async fn read_body(
    body: ::axum::body::Body,
    max_body_bytes: usize,
) -> ::std::result::Result<::axum::body::Bytes, RequestValidationRejection> {
    ::axum::body::to_bytes(body, max_body_bytes)
        .await
        .map_err(|error| {
            let source = ::std::error::Error::source(&error);
            if source
                .is_some_and(|source| {
                    source.is::<::http_body_util::LengthLimitError>()
                })
            {
                request_body_too_large()
            } else {
                malformed_request()
            }
        })
}
fn media_type_is(content_type: &str, expected: &str) -> bool {
    let Ok(content_type) = content_type.parse::<::mime::Mime>() else {
        return false;
    };
    let Ok(expected) = expected.parse::<::mime::Mime>() else {
        return false;
    };
    content_type.type_() == expected.type_()
        && content_type.subtype() == expected.subtype()
        && content_type.suffix() == expected.suffix()
        && expected
            .params()
            .all(|(name, value)| {
                content_type.get_param(name).is_some_and(|actual| actual == value)
            })
}
pub(crate) fn validate(
    target: &str,
    location: &str,
    instance: &::serde_json::Value,
) -> ::std::result::Result<(), RequestValidationRejection> {
    let validators = VALIDATORS.as_ref().map_err(|_| generated_contract_error())?;
    let Some(validator) = validators.get(target) else {
        return Err(generated_contract_error());
    };
    let mut errors: ::std::vec::Vec<InvalidParameter> = ::std::vec::Vec::new();
    for error in validator.iter_errors(instance) {
        let keyword = error.kind().keyword();
        let (code, message) = public_violation(keyword);
        let mut pointer = format!("{location}{}", error.instance_path());
        if let ::jsonschema::error::ValidationErrorKind::Required { property } = error
            .kind()
        {
            if let Some(property) = property.as_str() {
                pointer.push('/');
                pointer.push_str(&escape_pointer_token(property));
            }
        }
        let violation = InvalidParameter {
            code: code.to_string(),
            location: pointer,
            message: message.to_string(),
        };
        if !errors.contains(&violation) {
            errors.push(violation);
            if errors.len() == MAX_VALIDATION_ERRORS {
                break;
            }
        }
    }
    errors
        .sort_by(|left, right| {
            (&left.location, &left.code, &left.message)
                .cmp(&(&right.location, &right.code, &right.message))
        });
    if errors.is_empty() {
        return Ok(());
    }
    Err(
        RequestValidationRejection(ProblemDetails {
            r#type: "https://openapi-to-rust.dev/problems/validation".to_string(),
            title: "Request validation failed".to_string(),
            status: 422,
            code: "request_validation_failed".to_string(),
            errors,
        }),
    )
}
pub(crate) fn malformed_request() -> RequestValidationRejection {
    public_problem(
        400,
        "https://openapi-to-rust.dev/problems/malformed-request",
        "Malformed request",
        "malformed_request",
    )
}
pub(crate) fn malformed_parameter(location: &str) -> RequestValidationRejection {
    parameter_problem(
        400,
        "https://openapi-to-rust.dev/problems/malformed-parameter",
        "Malformed request parameter",
        "malformed_parameter",
        "malformed",
        location,
        "is malformed",
    )
}
pub(crate) fn missing_parameter(location: &str) -> RequestValidationRejection {
    parameter_problem(
        422,
        "https://openapi-to-rust.dev/problems/validation",
        "Request validation failed",
        "request_validation_failed",
        "required",
        location,
        "is required",
    )
}
fn schema_parameter_problem(
    location: &str,
    code: &str,
    message: &str,
) -> RequestValidationRejection {
    parameter_problem(
        422,
        "https://openapi-to-rust.dev/problems/validation",
        "Request validation failed",
        "request_validation_failed",
        code,
        location,
        message,
    )
}
fn parameter_problem(
    status: u16,
    problem_type: &str,
    title: &str,
    code: &str,
    error_code: &str,
    location: &str,
    message: &str,
) -> RequestValidationRejection {
    RequestValidationRejection(ProblemDetails {
        r#type: problem_type.to_string(),
        title: title.to_string(),
        status,
        code: code.to_string(),
        errors: vec![
            InvalidParameter { code : error_code.to_string(), location : location
            .to_string(), message : message.to_string(), }
        ],
    })
}
pub(crate) fn request_body_too_large() -> RequestValidationRejection {
    public_problem(
        413,
        "https://openapi-to-rust.dev/problems/request-body-too-large",
        "Request body too large",
        "request_body_too_large",
    )
}
pub(crate) fn unsupported_media_type() -> RequestValidationRejection {
    public_problem(
        415,
        "https://openapi-to-rust.dev/problems/unsupported-media-type",
        "Unsupported media type",
        "unsupported_media_type",
    )
}
pub(crate) fn generated_contract_error() -> RequestValidationRejection {
    public_problem(
        500,
        "https://openapi-to-rust.dev/problems/generated-contract-error",
        "Internal server error",
        "generated_contract_error",
    )
}
fn public_problem(
    status: u16,
    problem_type: &str,
    title: &str,
    code: &str,
) -> RequestValidationRejection {
    RequestValidationRejection(ProblemDetails {
        r#type: problem_type.to_string(),
        title: title.to_string(),
        status,
        code: code.to_string(),
        errors: Vec::new(),
    })
}
fn public_violation(keyword: &str) -> (&'static str, &'static str) {
    match keyword {
        "required" => ("required", "is required"),
        "type" => ("type", "has an invalid type"),
        "enum" => ("enum", "has an unsupported value"),
        "const" => ("const", "has an unsupported value"),
        "format" => ("format", "has an invalid format"),
        "pattern" => ("pattern", "does not match the required format"),
        "minLength" => ("min_length", "does not meet the length constraint"),
        "maxLength" => ("max_length", "does not meet the length constraint"),
        "minimum" => ("minimum", "is outside the allowed range"),
        "maximum" => ("maximum", "is outside the allowed range"),
        "exclusiveMinimum" => ("exclusive_minimum", "is outside the allowed range"),
        "exclusiveMaximum" => ("exclusive_maximum", "is outside the allowed range"),
        "multipleOf" => ("multiple_of", "is outside the allowed range"),
        "minItems" => ("min_items", "does not meet the item-count constraint"),
        "maxItems" => ("max_items", "does not meet the item-count constraint"),
        "uniqueItems" => ("unique_items", "contains duplicate items"),
        "minProperties" => {
            ("min_properties", "does not meet the property-count constraint")
        }
        "maxProperties" => {
            ("max_properties", "does not meet the property-count constraint")
        }
        "additionalProperties" => {
            ("additional_properties", "contains unsupported properties")
        }
        "unevaluatedProperties" => {
            ("unevaluated_properties", "contains unsupported properties")
        }
        "anyOf" => ("any_of", "does not match the required shape"),
        "oneOf" => ("one_of", "does not match the required shape"),
        "not" => ("not", "does not match the required shape"),
        _ => ("invalid_value", "is invalid"),
    }
}
fn escape_pointer_token(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
