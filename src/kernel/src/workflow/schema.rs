use serde_json::Value;

pub struct CompiledSchema {
    validator: jsonschema::Validator,
}

impl CompiledSchema {
    pub fn compile(schema: &Value) -> Result<Self, String> {
        check_references(schema, 0)?;
        let draft = match schema.get("$schema").and_then(Value::as_str) {
            None | Some("https://json-schema.org/draft/2020-12/schema") => {
                jsonschema::Draft::Draft202012
            }
            Some("https://json-schema.org/draft/2019-09/schema") => jsonschema::Draft::Draft201909,
            Some("http://json-schema.org/draft-07/schema#") => jsonschema::Draft::Draft7,
            Some("http://json-schema.org/draft-06/schema#") => jsonschema::Draft::Draft6,
            Some("http://json-schema.org/draft-04/schema#") => jsonschema::Draft::Draft4,
            Some(dialect) => return Err(format!("unsupported JSON Schema dialect: {dialect}")),
        };
        let validator = jsonschema::options()
            .with_draft(draft)
            .build(schema)
            .map_err(|error| format!("invalid JSON Schema: {error}"))?;
        Ok(Self { validator })
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        self.validator
            .validate(value)
            .map_err(|error| format!("{}: {error}", error.instance_path))
    }
}

fn check_references(schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("JSON Schema nesting exceeds 64 levels".into());
    }
    if let Some(object) = schema.as_object() {
        for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(reference) = object.get(key) {
                if !reference
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#'))
                {
                    return Err(format!(
                        "{key} must be a local fragment; external references are disabled"
                    ));
                }
            }
        }
        if object.contains_key("$schema") && depth > 0 {
            return Err("nested JSON Schema dialect changes are unsupported".into());
        }
        for key in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
        ] {
            if let Some(children) = object.get(key).and_then(Value::as_object) {
                for child in children.values() {
                    check_references(child, depth + 1)?;
                }
            }
        }
        for key in [
            "items",
            "additionalItems",
            "additionalProperties",
            "unevaluatedProperties",
            "unevaluatedItems",
            "contains",
            "propertyNames",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            if let Some(child) = object.get(key) {
                if let Some(children) = child.as_array() {
                    for child in children {
                        check_references(child, depth + 1)?;
                    }
                } else {
                    check_references(child, depth + 1)?;
                }
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(children) = object.get(key).and_then(Value::as_array) {
                for child in children {
                    check_references(child, depth + 1)?;
                }
            }
        }
        if let Some(children) = object.get("dependencies").and_then(Value::as_object) {
            for child in children
                .values()
                .filter(|value| value.is_object() || value.is_boolean())
            {
                check_references(child, depth + 1)?;
            }
        }
    }
    Ok(())
}
