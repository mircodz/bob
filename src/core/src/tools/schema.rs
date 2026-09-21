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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_nested_values_and_constraints() {
        let schema = CompiledSchema::compile(&json!({
            "type":"object", "required":["rows"], "additionalProperties":false,
            "properties":{"rows":{"type":"array", "minItems":1, "items":{
                "type":"object", "required":["count", "tag"], "additionalProperties":false,
                "properties":{"count":{"type":"integer", "minimum":1}, "tag":{"enum":["ok"]}}
            }}}
        }))
        .unwrap();
        assert!(schema
            .validate(&json!({"rows":[{"count":2.0,"tag":"ok"}]}))
            .is_ok());
        for (case, value) in [
            (
                "fractional integer",
                json!({"rows":[{"count":1.5,"tag":"ok"}]}),
            ),
            (
                "wrong property type",
                json!({"rows":[{"count":"nope","tag":"ok"}]}),
            ),
            ("below minimum", json!({"rows":[{"count":0,"tag":"ok"}]})),
            ("outside enum", json!({"rows":[{"count":1,"tag":"bad"}]})),
            ("missing required key", json!({"rows":[{"count":1}]})),
            ("empty array", json!({"rows":[]})),
            (
                "extra property",
                json!({"rows":[{"count":1,"tag":"ok"}],"extra":true}),
            ),
        ] {
            assert!(schema.validate(&value).is_err(), "{case}: {value}");
        }
    }

    #[test]
    fn local_refs_and_combiners_are_validated() {
        let schema = CompiledSchema::compile(&json!({
            "$defs":{"positive":{"type":"integer","minimum":1}},
            "anyOf":[{"$ref":"#/$defs/positive"},{"const":"skip"}]
        }))
        .unwrap();
        assert!(schema.validate(&json!(2)).is_ok());
        assert!(schema.validate(&json!("skip")).is_ok());
        assert!(schema.validate(&json!(0)).is_err());
    }

    #[test]
    fn external_refs_and_unknown_dialects_fail_without_retrieval() {
        for schema in [
            json!({"$ref":"file:///outside/schema.json"}),
            json!({"properties":{"x":{"$ref":"https://example.invalid/schema"}}}),
            json!({"$schema":"unknown"}),
            json!({"type":"not-a-type"}),
        ] {
            assert!(CompiledSchema::compile(&schema).is_err());
        }
        assert!(CompiledSchema::compile(&json!({"const":{"$ref":"literal data"}})).is_ok());
    }

    #[test]
    fn reference_guard_visits_every_schema_container_but_not_literal_data() {
        let external = json!({"$ref":"https://example.invalid/schema"});
        for key in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
            "dependencies",
        ] {
            let schema = json!({key: {"child": external.clone()}});
            assert!(
                check_references(&schema, 0)
                    .unwrap_err()
                    .contains("external references are disabled"),
                "{key}"
            );
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
            let schema = json!({key: external.clone()});
            assert!(check_references(&schema, 0).is_err(), "{key}");
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            let schema = json!({key: [true, external.clone()]});
            assert!(check_references(&schema, 0).is_err(), "{key}");
        }
        for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
            for reference in [json!("relative.json"), json!(false), Value::Null] {
                assert!(check_references(&json!({key: reference}), 0).is_err());
            }
            assert!(check_references(&json!({key: "#local"}), 0).is_ok());
        }
        let literal = json!({"$schema":"literal", "$ref":"not a reference"});
        let schema = CompiledSchema::compile(&json!({"const": literal.clone()})).unwrap();
        assert!(schema.validate(&literal).is_ok());
        assert!(schema.validate(&json!({})).is_err());
        assert!(check_references(&json!({"dependencies":{"x":["$ref", "$schema"]}}), 0).is_ok());
    }

    #[test]
    fn nesting_limit_and_nested_dialect_rejection_have_explicit_boundaries() {
        let mut schema = json!(true);
        for _ in 0..64 {
            schema = json!({"not": schema});
        }
        assert!(check_references(&schema, 0).is_ok());
        schema = json!({"not": schema});
        assert_eq!(
            check_references(&schema, 0).unwrap_err(),
            "JSON Schema nesting exceeds 64 levels"
        );
        let nested =
            json!({"properties":{"x":{"$schema":"https://json-schema.org/draft/2020-12/schema"}}});
        assert_eq!(
            CompiledSchema::compile(&nested).err().unwrap(),
            "nested JSON Schema dialect changes are unsupported"
        );
    }

    #[test]
    fn supported_dialects_validate_and_boolean_schemas_keep_their_meaning() {
        for dialect in [
            "https://json-schema.org/draft/2020-12/schema",
            "https://json-schema.org/draft/2019-09/schema",
            "http://json-schema.org/draft-07/schema#",
            "http://json-schema.org/draft-06/schema#",
            "http://json-schema.org/draft-04/schema#",
        ] {
            let schema = CompiledSchema::compile(
                &json!({"$schema": dialect, "type":"integer", "minimum":2}),
            )
            .unwrap();
            assert!(schema.validate(&json!(2)).is_ok(), "{dialect}");
            assert!(schema.validate(&json!(1)).is_err(), "{dialect}");
            assert!(schema.validate(&json!("2")).is_err(), "{dialect}");
        }
        let accept = CompiledSchema::compile(&json!(true)).unwrap();
        let reject = CompiledSchema::compile(&json!(false)).unwrap();
        for value in [Value::Null, json!({}), json!([1, "x"])] {
            assert!(accept.validate(&value).is_ok());
            assert!(reject.validate(&value).is_err());
        }
    }

    #[test]
    fn validation_errors_identify_the_nested_instance_path() {
        let schema = CompiledSchema::compile(&json!({
            "properties":{"rows":{"items":{"properties":{"count":{"type":"integer"}}}}}
        }))
        .unwrap();
        let error = schema
            .validate(&json!({"rows":[{"count":"wrong"}]}))
            .unwrap_err();
        assert!(error.starts_with("/rows/0/count:"), "{error}");
    }
}
