//! Structural signatures for storage JSON Schemas.
//!
//! A schema's compatibility-relevant structure is flattened to one signature
//! per property path (`<type> required|optional[ enum:a|b]`), which the
//! allocation registry persists and compares across revisions. The walk
//! understands inline `type`/`properties`/`required`/`items`/`enum` schemas;
//! storage schemas are authored without `$ref` so this stays total.

use serde_yaml::Value;

use crate::allocations::SchemaShape;

pub fn schema_shape(root: &Value) -> Result<SchemaShape, String> {
    if !matches!(root, Value::Mapping(_)) {
        return Err("schema document is not a mapping".to_owned());
    }
    let mut shape = SchemaShape::new();
    walk(root, "", true, &mut shape)?;
    Ok(shape)
}

fn walk(schema: &Value, path: &str, required: bool, shape: &mut SchemaShape) -> Result<(), String> {
    if schema.get("$ref").is_some() {
        return Err(format!(
            "schema path {path:?} uses $ref: storage schemas are authored inline"
        ));
    }
    let described = schema
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if !described {
        let name = if path.is_empty() { "the root" } else { path };
        return Err(format!(
            "schema node {name} has no description: storage schemas document every field"
        ));
    }
    let type_name = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("any")
        .to_owned();
    let mut signature = format!(
        "{type_name} {}",
        if required { "required" } else { "optional" }
    );
    if let Some(values) = schema.get("enum").and_then(Value::as_sequence) {
        let rendered = values
            .iter()
            .map(|value| match value {
                Value::String(text) => Ok(text.clone()),
                Value::Number(number) => Ok(number.to_string()),
                Value::Bool(value) => Ok(value.to_string()),
                other => Err(format!(
                    "schema path {path:?} has non-scalar enum {other:?}"
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        signature.push_str(&format!(" enum:{}", rendered.join("|")));
    }
    if !path.is_empty() {
        shape.insert(path.to_owned(), signature);
    }

    let required_names: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_sequence)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if let Some(properties) = schema.get("properties").and_then(Value::as_mapping) {
        for (name, child) in properties {
            let Some(name) = name.as_str() else {
                return Err(format!("schema path {path:?} has a non-string property"));
            };
            let child_path = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}.{name}")
            };
            walk(child, &child_path, required_names.contains(&name), shape)?;
        }
    }
    if let Some(items) = schema.get("items") {
        walk(items, &format!("{path}[]"), false, shape)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_properties_arrays_and_enums() {
        let schema: Value = serde_yaml::from_str(
            r#"
            description: The envelope.
            type: object
            required: [format, record]
            properties:
              format:
                description: The format.
                type: integer
                enum: [1]
              record:
                description: The record.
                type: object
                required: [id]
                properties:
                  id: { description: The identity., type: string }
                  columns:
                    description: The columns.
                    type: array
                    items:
                      description: One column.
                      type: object
                      properties:
                        name: { description: The name., type: string }
            "#,
        )
        .unwrap();
        let shape = schema_shape(&schema).unwrap();
        assert_eq!(shape["format"], "integer required enum:1");
        assert_eq!(shape["record"], "object required");
        assert_eq!(shape["record.id"], "string required");
        assert_eq!(shape["record.columns"], "array optional");
        assert_eq!(shape["record.columns[]"], "object optional");
        assert_eq!(shape["record.columns[].name"], "string optional");
    }

    #[test]
    fn rejects_refs() {
        let schema: Value = serde_yaml::from_str(
            r##"{ description: D., type: object, properties: { x: { description: X., "$ref": "#/x" } } }"##,
        )
        .unwrap();
        assert!(schema_shape(&schema).unwrap_err().contains("$ref"));
    }

    #[test]
    fn rejects_undocumented_nodes() {
        let schema: Value = serde_yaml::from_str(
            r#"
            description: The envelope.
            type: object
            properties:
              format: { type: integer }
            "#,
        )
        .unwrap();
        let error = schema_shape(&schema).unwrap_err();
        assert!(error.contains("format"), "{error}");
        assert!(error.contains("no description"), "{error}");
    }
}
