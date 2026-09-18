use serde_json::{json, Map, Value};

const NAME_PATTERN: &str = "^[A-Za-z][A-Za-z0-9_-]*$";
const REFERENCE_PATTERN: &str = r"^\$[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z0-9_-]+)*$";
const OPERATIONS: [&str; 4] = ["agent", "fan_out", "parallel", "loop"];
const MAX_COUNT: u64 = 20;
const REDUCE_DEFAULT: &str = "Synthesize these results into a single summary.";
const FIND_DEFAULT: &str =
    "Find items not already in the seen list; return {findings:[..]}, empty if none new.";

/// Describe the existing shape and steps forms, including recursive operations.
pub fn schema() -> Value {
    let mut definitions = json!({
        "name": {
            "type": "string",
            "pattern": NAME_PATTERN,
            "description": "Step IDs and branch names start with an ASCII letter, followed by ASCII letters, digits, underscores or hyphens. Dots, dollar signs and leading underscores are not allowed."
        },
        "reference": {
            "type": "string",
            "pattern": REFERENCE_PATTERN,
            "description": "A reference is $step or $step.field.subfield. The step uses the name grammar; each dot-separated field is one or more ASCII letters, digits, underscores or hyphens. Metadata fields and numeric object keys are allowed; numeric segments are not array indices. No braces, empty segments or whitespace. Visibility is checked separately by the DSL."
        },
        "prompt": {
            "type": "string",
            "pattern": "\\S",
            "description": "Nonblank instructions. In steps, {$step} or {$step.field.subfield} interpolates a reference using the reference grammar. In fan_out, {item} inserts the item; {item.field} and {item.field.nested} read object field paths. Shape map_prompt substitutes {item} only. Other prompt text is unrestricted."
        },
        "output_schema": {
            "type": "object",
            "description": "An arbitrary JSON Schema object for agent output. Its contents are passed through, not restricted to a fixed deliverable or validated as an external schema."
        },
        "items": {
            "type": "array",
            "minItems": 1,
            "items": {"type": "string"}
        },
        "steps": {
            "type": "array",
            "minItems": 1,
            "items": {"$ref": "#/$defs/step"}
        }
    });
    definitions["agent"] = object_schema(
        json!({
            "prompt": {"$ref": "#/$defs/prompt"},
            "schema": {"$ref": "#/$defs/output_schema"}
        }),
        &["prompt"],
    );
    definitions["fan_out"] = object_schema(
        json!({
            "over": {
                "description": "An inline array of arbitrary JSON items (possibly empty), or a reference to a prior output.",
                "oneOf": [
                    {"type": "array", "items": {}},
                    {"$ref": "#/$defs/reference"}
                ]
            },
            "prompt": {"$ref": "#/$defs/prompt"},
            "schema": {"$ref": "#/$defs/output_schema"},
            "repeat": {
                "type": "integer", "minimum": 1, "maximum": MAX_COUNT, "default": 1,
                "description": "Agents per item. Limited to 20 to bound duplicate reviewers."
            }
        }),
        &["over", "prompt"],
    );
    definitions["parallel"] = object_schema(
        json!({
            "branches": {
                "type": "object",
                "minProperties": 1,
                "patternProperties": {NAME_PATTERN: {"$ref": "#/$defs/operation"}},
                "additionalProperties": false,
                "description": "Nonempty named branches. Each value is exactly one operation without an id; operations may nest recursively."
            }
        }),
        &["branches"],
    );
    definitions["loop"] = object_schema(
        json!({
            "steps": {"$ref": "#/$defs/steps"},
            "until": {
                "$ref": "#/$defs/reference",
                "description": "Reference to a boolean output. Stop when it is true; false continues until max rounds. Omit to run max rounds."
            },
            "max": {"type": "integer", "minimum": 1, "maximum": MAX_COUNT, "default": 5}
        }),
        &["steps"],
    );
    definitions["step"] = operation_schema(true);
    definitions["operation"] = operation_schema(false);

    let common = json!({
        "title": {"type": "string", "description": "Short label for the workflow run."},
        "read_only": {
            "type": "boolean", "default": false,
            "description": "Restrict the workflow's agents to read-only tools."
        }
    });
    let mut pipeline = common.clone();
    pipeline["steps"] = json!({"$ref": "#/$defs/steps"});
    definitions["steps_form"] = object_schema(pipeline, &["steps"]);

    let mut fan_out = common.clone();
    fan_out["shape"] = json!({"type": "string", "const": "fan_out"});
    fan_out["items"] = json!({"$ref": "#/$defs/items"});
    fan_out["map_prompt"] = json!({"$ref": "#/$defs/prompt"});
    fan_out["map_schema"] = json!({"$ref": "#/$defs/output_schema"});
    definitions["fan_out_shape"] =
        object_schema(fan_out.clone(), &["shape", "items", "map_prompt"]);

    let mut map_reduce = fan_out;
    map_reduce["shape"] = json!({"type": "string", "const": "map_reduce"});
    map_reduce["reduce_prompt"] = json!({"$ref": "#/$defs/prompt", "default": REDUCE_DEFAULT});
    map_reduce["reduce_schema"] = json!({"$ref": "#/$defs/output_schema"});
    definitions["map_reduce_shape"] = object_schema(map_reduce, &["shape", "items", "map_prompt"]);

    let mut loop_shape = common;
    loop_shape["shape"] = json!({"type": "string", "const": "loop"});
    loop_shape["find_prompt"] = json!({"$ref": "#/$defs/prompt", "default": FIND_DEFAULT});
    loop_shape["max_rounds"] =
        json!({"type": "integer", "minimum": 1, "maximum": MAX_COUNT, "default": 5});
    definitions["loop_shape"] = object_schema(loop_shape, &["shape"]);

    let forms = [
        "steps_form",
        "fan_out_shape",
        "map_reduce_shape",
        "loop_shape",
    ];
    let mut properties = Map::new();
    for form in forms {
        properties.extend(definitions[form]["properties"].as_object().unwrap().clone());
    }
    properties.insert(
        "shape".into(),
        json!({"type": "string", "enum": ["fan_out", "map_reduce", "loop"]}),
    );
    let mut root = object_schema(Value::Object(properties), &[]);
    root["description"] = json!("Exactly one form: steps, or a canned shape. Only fields belonging to that form and shape are accepted.");
    root["oneOf"] = Value::Array(
        forms
            .iter()
            .map(|name| json!({"$ref": format!("#/$defs/{name}")}))
            .collect(),
    );
    root["$defs"] = definitions;
    root
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn operation_schema(named: bool) -> Value {
    let mut properties = Map::new();
    for op in OPERATIONS {
        properties.insert(op.into(), json!({"$ref": format!("#/$defs/{op}")}));
    }
    if named {
        properties.insert("id".into(), json!({"$ref": "#/$defs/name"}));
    }
    let required: &[&str] = if named { &["id"] } else { &[] };
    let mut schema = object_schema(Value::Object(properties), required);
    schema["oneOf"] = Value::Array(
        OPERATIONS
            .iter()
            .map(|op| json!({"required": [op]}))
            .collect(),
    );
    schema
}

/// Validate input structure before execution, without resolving reference visibility.
/// Output schemas must be objects; their JSON Schema contents are not validated.
/// `repeat` is capped at 20 to bound duplicate reviewers, like the loop round caps.
pub fn validate(input: &Value) -> Result<(), String> {
    let object = as_object(input, "$")?;
    if let Some(title) = object.get("title") {
        as_string(title, "$.title")?;
    }
    if let Some(read_only) = object.get("read_only") {
        if !read_only.is_boolean() {
            return Err("$.read_only must be a boolean".into());
        }
    }
    match (object.contains_key("steps"), object.contains_key("shape")) {
        (true, false) => {
            fields(object, &["title", "read_only", "steps"], "$")?;
            validate_steps(&object["steps"], "$.steps")
        }
        (false, true) => validate_shape(object),
        _ => Err("$ must contain exactly one of steps or shape".into()),
    }
}

fn validate_shape(object: &Map<String, Value>) -> Result<(), String> {
    let shape = as_string(&object["shape"], "$.shape")?;
    let allowed: &[&str] = match shape {
        "fan_out" => &[
            "title",
            "read_only",
            "shape",
            "items",
            "map_prompt",
            "map_schema",
        ],
        "map_reduce" => &[
            "title",
            "read_only",
            "shape",
            "items",
            "map_prompt",
            "map_schema",
            "reduce_prompt",
            "reduce_schema",
        ],
        "loop" => &["title", "read_only", "shape", "find_prompt", "max_rounds"],
        _ => return Err("$.shape must be fan_out, map_reduce or loop".into()),
    };
    fields(object, allowed, "$")?;
    if shape == "loop" {
        optional_prompt(object, "find_prompt", "$")?;
        optional_count(object, "max_rounds", "$")?;
    } else {
        let items = nonempty_array(required(object, "items", "$")?, "$.items")?;
        for (index, item) in items.iter().enumerate() {
            as_string(item, &format!("$.items[{index}]"))?;
        }
        prompt(required(object, "map_prompt", "$")?, "$.map_prompt")?;
        optional_schema(object, "map_schema", "$")?;
        if shape == "map_reduce" {
            optional_prompt(object, "reduce_prompt", "$")?;
            optional_schema(object, "reduce_schema", "$")?;
        }
    }
    Ok(())
}

fn validate_steps(value: &Value, path: &str) -> Result<(), String> {
    for (index, step) in nonempty_array(value, path)?.iter().enumerate() {
        validate_operation(step, &format!("{path}[{index}]"), true)?;
    }
    Ok(())
}

fn validate_operation(value: &Value, path: &str, named: bool) -> Result<(), String> {
    let object = as_object(value, path)?;
    if named {
        fields(
            object,
            &["id", "agent", "fan_out", "parallel", "loop"],
            path,
        )?;
        let id = as_string(required(object, "id", path)?, &format!("{path}.id"))?;
        validate_name(id, &format!("{path}.id"))?;
    } else {
        fields(object, &OPERATIONS, path)?;
    }
    let mut ops = OPERATIONS.iter().filter(|op| object.contains_key(**op));
    let op = ops.next().copied();
    if op.is_none() || ops.next().is_some() {
        return Err(format!(
            "{path} must contain exactly one operation: agent, fan_out, parallel or loop"
        ));
    }
    let op = op.unwrap();
    let path = format!("{path}.{op}");
    let body = as_object(&object[op], &path)?;
    match op {
        "agent" => {
            fields(body, &["prompt", "schema"], &path)?;
            prompt(required(body, "prompt", &path)?, &format!("{path}.prompt"))?;
            optional_schema(body, "schema", &path)?;
        }
        "fan_out" => {
            fields(body, &["over", "prompt", "schema", "repeat"], &path)?;
            let over = required(body, "over", &path)?;
            if !over.is_array() {
                reference(over, &format!("{path}.over"))?;
            }
            prompt(required(body, "prompt", &path)?, &format!("{path}.prompt"))?;
            optional_schema(body, "schema", &path)?;
            optional_count(body, "repeat", &path)?;
        }
        "parallel" => {
            fields(body, &["branches"], &path)?;
            let branches = required(body, "branches", &path)?;
            let path = format!("{path}.branches");
            let branches = as_object(branches, &path)?;
            if branches.is_empty() {
                return Err(format!("{path} must not be empty"));
            }
            for (name, branch) in branches {
                validate_name(name, &format!("{path} branch name {name:?}"))?;
                validate_operation(branch, &format!("{path}.{name}"), false)?;
            }
        }
        "loop" => {
            fields(body, &["steps", "until", "max"], &path)?;
            validate_steps(required(body, "steps", &path)?, &format!("{path}.steps"))?;
            if let Some(until) = body.get("until") {
                reference(until, &format!("{path}.until"))?;
            }
            optional_count(body, "max", &path)?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn as_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))
}

fn as_string<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{path} must be a string"))
}

fn nonempty_array<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| format!("{path} must be an array"))?;
    if array.is_empty() {
        return Err(format!("{path} must not be empty"));
    }
    Ok(array)
}

fn fields(object: &Map<String, Value>, allowed: &[&str], path: &str) -> Result<(), String> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path}.{key} is not an allowed field"));
        }
    }
    Ok(())
}

fn required<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("{path}.{key} is required"))
}

fn prompt(value: &Value, path: &str) -> Result<(), String> {
    if as_string(value, path)?.trim().is_empty() {
        return Err(format!("{path} must not be blank"));
    }
    Ok(())
}

fn optional_prompt(object: &Map<String, Value>, key: &str, path: &str) -> Result<(), String> {
    if let Some(value) = object.get(key) {
        prompt(value, &format!("{path}.{key}"))?;
    }
    Ok(())
}

fn optional_schema(object: &Map<String, Value>, key: &str, path: &str) -> Result<(), String> {
    if let Some(value) = object.get(key) {
        as_object(value, &format!("{path}.{key}"))?;
    }
    Ok(())
}

fn optional_count(object: &Map<String, Value>, key: &str, path: &str) -> Result<(), String> {
    if let Some(value) = object.get(key) {
        if !value
            .as_u64()
            .is_some_and(|count| (1..=MAX_COUNT).contains(&count))
        {
            return Err(format!(
                "{path}.{key} must be an integer in 1..={MAX_COUNT}"
            ));
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn validate_name(name: &str, path: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("{path} must match {NAME_PATTERN}"));
    }
    Ok(())
}

fn reference(value: &Value, path: &str) -> Result<(), String> {
    let text = as_string(value, path)?;
    let valid = text.strip_prefix('$').is_some_and(|rest| {
        let mut segments = rest.split('.');
        valid_name(segments.next().unwrap_or_default())
            && segments.all(|field| {
                !field.is_empty()
                    && field
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            })
    });
    if !valid {
        return Err(format!(
            "{path} must match {REFERENCE_PATTERN} ($step or $step.field)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline(op: Value) -> Value {
        let mut step = op.as_object().unwrap().clone();
        step.insert("id".into(), json!("work"));
        json!({"steps": [step]})
    }

    fn agent() -> Value {
        json!({"agent": {"prompt": "Review the code"}})
    }

    fn shape(name: &str) -> Value {
        if name == "loop" {
            json!({"shape": name})
        } else {
            json!({"shape": name, "items": ["core"], "map_prompt": "Review {item}"})
        }
    }

    fn accepts(value: &Value) {
        assert_eq!(validate(value), Ok(()), "input: {value}");
    }

    fn rejects(value: &Value) {
        assert!(validate(value).is_err(), "unexpectedly accepted: {value}");
    }

    #[test]
    fn accepts_complete_nested_steps() {
        accepts(&json!({
            "title": "review-crates",
            "read_only": true,
            "steps": [
                {"id": "crates", "agent": {
                    "prompt": "List all crates",
                    "schema": {"type": "object", "properties": {
                        "list": {"type": "array", "items": {"type": "string"}}
                    }, "required": ["list"]}
                }},
                {"id": "review-2", "parallel": {"branches": {
                    "reviews": {"fan_out": {
                        "over": "$crates.list", "repeat": 20,
                        "prompt": "Review {item} using {$crates.list}", "schema": {}
                    }},
                    "nested_branch": {"parallel": {"branches": {
                        "check": {"loop": {
                            "max": 20, "until": "$coverage.pass",
                            "steps": [
                                {"id": "coverage", "agent": {
                                    "prompt": "Check coverage", "schema": {"type": "boolean"}
                                }},
                                {"id": "inner", "loop": {"steps": [
                                    {"id": "items", "fan_out": {
                                        "over": [{"name": "core"}, "cli", 3, true, null, []],
                                        "prompt": "Inspect {item.name}", "repeat": 1
                                    }}
                                ]}}
                            ]
                        }}
                    }}}
                }}}
            ]
        }));
    }

    #[test]
    fn accepts_all_shapes_with_defaults_and_complete_fields() {
        for name in ["fan_out", "map_reduce", "loop"] {
            accepts(&shape(name));
        }
        accepts(&json!({
            "shape": "fan_out", "title": "inspect", "read_only": true,
            "items": ["a", "b"], "map_prompt": "Inspect {item}",
            "map_schema": {"type": "array", "items": {"type": "string"}}
        }));
        accepts(&json!({
            "shape": "map_reduce", "title": "summarize", "read_only": false,
            "items": ["a", "b"], "map_prompt": "Inspect {item}", "map_schema": {},
            "reduce_prompt": "Combine the reports", "reduce_schema": {"custom": true}
        }));
        accepts(&json!({
            "shape": "loop", "title": "discover", "read_only": true,
            "find_prompt": "Find new problems", "max_rounds": 20
        }));
    }

    #[test]
    fn permits_arbitrary_output_schema_objects_and_empty_inline_arrays() {
        for schema in [
            json!({}),
            json!({"type": "not-a-real-type", "arbitrary": null}),
        ] {
            accepts(&pipeline(
                json!({"agent": {"prompt": "Inspect", "schema": schema}}),
            ));
        }
        accepts(&pipeline(
            json!({"fan_out": {"over": [], "prompt": "Inspect {item}"}}),
        ));
        accepts(&pipeline(json!({"loop": {"steps": [{"id": "a", "agent": {
            "prompt": "Literal {braces}, {$future.field}, and {item.field}"
        }}]}})));
    }

    #[test]
    fn rejects_invalid_forms_and_root_types() {
        for value in [
            Value::Null,
            json!(false),
            json!(3),
            json!("loop"),
            json!([]),
            json!({}),
            json!({"shape": "unknown"}),
            json!({"shape": null}),
            json!({"steps": []}),
            json!({"steps": null}),
            json!({"steps": {}}),
            json!({"steps": [null]}),
            json!({"steps": [true]}),
            json!({"shape": "loop", "steps": []}),
            json!({"shape": null, "steps": null}),
        ] {
            rejects(&value);
        }
        let mut mixed = pipeline(agent());
        mixed["shape"] = json!("loop");
        rejects(&mixed);
        for (key, values) in [
            (
                "title",
                vec![Value::Null, json!(false), json!(2), json!([]), json!({})],
            ),
            (
                "read_only",
                vec![Value::Null, json!("true"), json!(1), json!([]), json!({})],
            ),
        ] {
            for value in values {
                for mut input in [pipeline(agent()), shape("loop")] {
                    input[key] = value.clone();
                    rejects(&input);
                }
            }
        }
    }

    #[test]
    fn rejects_missing_and_multiple_operations_and_ids() {
        for step in [
            json!({}),
            json!({"id": "a"}),
            agent(),
            json!({"id": null, "agent": {"prompt": "Review"}}),
            json!({"id": "a", "unknown": {}}),
            json!({"id": "a", "agent": {"prompt": "Review"}, "loop": {"steps": []}}),
        ] {
            rejects(&json!({"steps": [step]}));
        }
        for op in OPERATIONS {
            for body in [
                Value::Null,
                json!(true),
                json!(1),
                json!("bad"),
                json!([]),
                json!({}),
            ] {
                rejects(&pipeline(json!({op: body})));
            }
        }
        rejects(&pipeline(json!({"fan_out": {"over": []}})));
        rejects(&pipeline(json!({"fan_out": {"prompt": "Review"}})));
    }

    #[test]
    fn rejects_invalid_nested_branches_before_execution() {
        for branch in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"unknown": {}}),
            json!({"agent": {"prompt": " "}}),
            json!({"id": "not-allowed", "agent": {"prompt": "Inspect"}}),
            json!({"agent": {"prompt": "Inspect"}, "fan_out": {"over": [], "prompt": "Inspect"}}),
            json!({"loop": {"steps": []}}),
            json!({"parallel": {"branches": {"deep": {"agent": {"prompt": null}}}}}),
        ] {
            rejects(&pipeline(
                json!({"parallel": {"branches": {"review": branch}}}),
            ));
        }
        for branches in [Value::Null, json!([]), json!({}), json!("bad")] {
            rejects(&pipeline(json!({"parallel": {"branches": branches}})));
        }
        for steps in [Value::Null, json!({}), json!([]), json!([{}])] {
            rejects(&pipeline(json!({"loop": {"steps": steps}})));
        }
        let invalid = pipeline(json!({"parallel": {"branches": {"review": {
            "agent": {"prompt": null}
        }}}}));
        assert!(validate(&invalid)
            .unwrap_err()
            .contains("$.steps[0].parallel.branches.review.agent.prompt"));
    }

    #[test]
    fn rejects_extraneous_fields_at_every_structural_level() {
        let mut steps = pipeline(agent());
        steps["items"] = json!(["a"]);
        rejects(&steps);
        for mut input in [
            pipeline(agent()),
            shape("fan_out"),
            shape("map_reduce"),
            shape("loop"),
        ] {
            input["unexpected"] = json!(true);
            rejects(&input);
        }
        for (name, key) in [
            ("fan_out", "reduce_prompt"),
            ("fan_out", "reduce_schema"),
            ("fan_out", "find_prompt"),
            ("fan_out", "max_rounds"),
            ("map_reduce", "find_prompt"),
            ("map_reduce", "max_rounds"),
            ("loop", "items"),
            ("loop", "map_prompt"),
            ("loop", "map_schema"),
            ("loop", "reduce_prompt"),
            ("loop", "reduce_schema"),
        ] {
            let mut input = shape(name);
            input[key] = Value::Null;
            rejects(&input);
        }
        let mut input = pipeline(agent());
        input["steps"][0]["read_only"] = json!(true);
        rejects(&input);
        for mut op in [
            agent(),
            json!({"fan_out": {"over": [], "prompt": "Inspect"}}),
            json!({"parallel": {"branches": {"review": agent()}}}),
            json!({"loop": {"steps": pipeline(agent())["steps"]}}),
        ] {
            let name = op.as_object().unwrap().keys().next().unwrap().clone();
            op[&name]["unexpected"] = json!(true);
            rejects(&pipeline(op));
        }
    }

    #[test]
    fn rejects_invalid_prompts_items_and_output_schemas() {
        for value in [
            Value::Null,
            json!(false),
            json!(4),
            json!([]),
            json!({}),
            json!(""),
            json!(" \n\t"),
        ] {
            rejects(&pipeline(json!({"agent": {"prompt": value}})));
            rejects(&pipeline(json!({"fan_out": {"over": [], "prompt": value}})));
            for (name, key) in [
                ("fan_out", "map_prompt"),
                ("map_reduce", "map_prompt"),
                ("map_reduce", "reduce_prompt"),
                ("loop", "find_prompt"),
            ] {
                let mut input = shape(name);
                input[key] = value.clone();
                rejects(&input);
            }
        }
        for name in ["fan_out", "map_reduce"] {
            for key in ["items", "map_prompt"] {
                let mut input = shape(name);
                input.as_object_mut().unwrap().remove(key);
                rejects(&input);
            }
            for items in [
                Value::Null,
                json!({}),
                json!("a"),
                json!([]),
                json!([null]),
                json!([1]),
                json!([{}]),
            ] {
                let mut input = shape(name);
                input["items"] = items;
                rejects(&input);
            }
        }
        for value in [
            Value::Null,
            json!(false),
            json!(4),
            json!([]),
            json!("schema"),
        ] {
            rejects(&pipeline(
                json!({"agent": {"prompt": "Inspect", "schema": value}}),
            ));
            rejects(&pipeline(
                json!({"fan_out": {"over": [], "prompt": "Inspect", "schema": value}}),
            ));
            for (name, key) in [
                ("fan_out", "map_schema"),
                ("map_reduce", "map_schema"),
                ("map_reduce", "reduce_schema"),
            ] {
                let mut input = shape(name);
                input[key] = value.clone();
                rejects(&input);
            }
        }
    }

    #[test]
    fn enforces_count_limits_and_types() {
        for count in [
            Value::Null,
            json!(true),
            json!("2"),
            json!([]),
            json!({}),
            json!(-1),
            json!(0),
            json!(21),
            json!(1.5),
            json!(u64::MAX),
        ] {
            rejects(&pipeline(
                json!({"fan_out": {"over": [], "prompt": "Inspect", "repeat": count}}),
            ));
            rejects(&pipeline(
                json!({"loop": {"steps": pipeline(agent())["steps"], "max": count}}),
            ));
            rejects(&json!({"shape": "loop", "max_rounds": count}));
        }
        for count in [1, 20] {
            accepts(&pipeline(
                json!({"fan_out": {"over": [], "prompt": "Inspect", "repeat": count}}),
            ));
            accepts(&pipeline(
                json!({"loop": {"steps": pipeline(agent())["steps"], "max": count}}),
            ));
            accepts(&json!({"shape": "loop", "max_rounds": count}));
        }
    }

    #[test]
    fn enforces_name_grammar_for_ids_and_branches() {
        let pattern = regex::Regex::new(NAME_PATTERN).unwrap();
        for (name, valid) in [
            ("A", true),
            ("review_2-b", true),
            ("", false),
            ("2a", false),
            ("_iterations", false),
            ("a.b", false),
            ("$a", false),
            ("a b", false),
            ("a\n", false),
            ("é", false),
        ] {
            let mut input = pipeline(agent());
            input["steps"][0]["id"] = json!(name);
            assert_eq!(validate(&input).is_ok(), valid, "id: {name:?}");
            let input = pipeline(json!({"parallel": {"branches": {name: agent()}}}));
            assert_eq!(validate(&input).is_ok(), valid, "branch: {name:?}");
            assert_eq!(pattern.is_match(name), valid, "schema pattern: {name:?}");
        }
    }

    #[test]
    fn validates_reference_syntax_without_checking_visibility() {
        let pattern = regex::Regex::new(REFERENCE_PATTERN).unwrap();
        for (text, valid) in [
            ("$future", true),
            ("$step.field.subfield", true),
            ("$step-2._iterations", true),
            ("$step._stop_reason", true),
            ("$step.0", true),
            ("", false),
            ("step", false),
            ("$", false),
            ("$$step", false),
            ("$step.", false),
            ("$step..field", false),
            ("$.field", false),
            ("$step[0]", false),
            ("{$step.field}", false),
            ("$step.$field", false),
            ("$step.field name", false),
            ("$step\n", false),
            ("$_iterations", false),
        ] {
            let over = pipeline(json!({"fan_out": {"over": text, "prompt": "Inspect"}}));
            let until =
                pipeline(json!({"loop": {"steps": pipeline(agent())["steps"], "until": text}}));
            assert_eq!(validate(&over).is_ok(), valid, "over: {text:?}");
            assert_eq!(validate(&until).is_ok(), valid, "until: {text:?}");
            assert_eq!(pattern.is_match(text), valid, "schema pattern: {text:?}");
        }
        for value in [Value::Null, json!(false), json!(3), json!({})] {
            rejects(&pipeline(
                json!({"fan_out": {"over": value, "prompt": "Inspect"}}),
            ));
            rejects(&pipeline(
                json!({"loop": {"steps": pipeline(agent())["steps"], "until": value}}),
            ));
        }
        rejects(&pipeline(
            json!({"loop": {"steps": pipeline(agent())["steps"], "until": []}}),
        ));
    }

    fn assert_object(schema: &Value, required: &[&str], properties: &[&str]) {
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(required));
        let actual = schema["properties"].as_object().unwrap();
        assert_eq!(actual.len(), properties.len());
        for key in properties {
            assert!(actual.contains_key(*key), "missing property: {key}");
        }
    }

    #[test]
    fn schema_describes_closed_exclusive_forms_and_defaults() {
        let schema = schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["read_only"]["type"], "boolean");
        assert_eq!(schema["properties"]["read_only"]["default"], false);
        assert_eq!(
            schema["oneOf"],
            json!([
                {"$ref": "#/$defs/steps_form"}, {"$ref": "#/$defs/fan_out_shape"},
                {"$ref": "#/$defs/map_reduce_shape"}, {"$ref": "#/$defs/loop_shape"}
            ])
        );
        let defs = &schema["$defs"];
        assert_object(
            &defs["steps_form"],
            &["steps"],
            &["title", "read_only", "steps"],
        );
        assert_object(
            &defs["fan_out_shape"],
            &["shape", "items", "map_prompt"],
            &[
                "title",
                "read_only",
                "shape",
                "items",
                "map_prompt",
                "map_schema",
            ],
        );
        assert_object(
            &defs["map_reduce_shape"],
            &["shape", "items", "map_prompt"],
            &[
                "title",
                "read_only",
                "shape",
                "items",
                "map_prompt",
                "map_schema",
                "reduce_prompt",
                "reduce_schema",
            ],
        );
        assert_object(
            &defs["loop_shape"],
            &["shape"],
            &["title", "read_only", "shape", "find_prompt", "max_rounds"],
        );
        for (definition, name) in [
            ("fan_out_shape", "fan_out"),
            ("map_reduce_shape", "map_reduce"),
            ("loop_shape", "loop"),
        ] {
            assert_eq!(defs[definition]["properties"]["shape"]["const"], name);
        }
        assert_eq!(
            defs["map_reduce_shape"]["properties"]["reduce_prompt"]["default"],
            REDUCE_DEFAULT
        );
        assert_eq!(
            defs["loop_shape"]["properties"]["find_prompt"]["default"],
            FIND_DEFAULT
        );
    }

    #[test]
    fn schema_covers_recursive_operations_required_fields_and_constraints() {
        let schema = schema();
        let defs = &schema["$defs"];
        assert_object(&defs["agent"], &["prompt"], &["prompt", "schema"]);
        assert_object(
            &defs["fan_out"],
            &["over", "prompt"],
            &["over", "prompt", "schema", "repeat"],
        );
        assert_object(&defs["parallel"], &["branches"], &["branches"]);
        assert_object(&defs["loop"], &["steps"], &["steps", "until", "max"]);
        assert_object(
            &defs["step"],
            &["id"],
            &["id", "agent", "fan_out", "parallel", "loop"],
        );
        assert_object(&defs["operation"], &[], &OPERATIONS);
        for definition in ["step", "operation"] {
            let wrapper = &defs[definition];
            assert_eq!(wrapper["oneOf"].as_array().unwrap().len(), OPERATIONS.len());
            for (index, op) in OPERATIONS.iter().enumerate() {
                assert_eq!(wrapper["oneOf"][index], json!({"required": [op]}));
                assert_eq!(wrapper["properties"][*op]["$ref"], format!("#/$defs/{op}"));
            }
        }
        assert_eq!(defs["step"]["properties"]["id"]["$ref"], "#/$defs/name");
        assert_eq!(defs["name"]["pattern"], NAME_PATTERN);
        assert_eq!(defs["reference"]["pattern"], REFERENCE_PATTERN);
        assert_eq!(defs["prompt"]["pattern"], "\\S");
        let description = defs["prompt"]["description"].as_str().unwrap();
        assert!(description.contains("{$step.field.subfield}"));
        assert!(description.contains("{item.field}"));
        for definition in ["items", "steps"] {
            assert_eq!(defs[definition]["type"], "array");
            assert_eq!(defs[definition]["minItems"], 1);
        }
        assert_eq!(defs["items"]["items"]["type"], "string");
        assert_eq!(defs["steps"]["items"]["$ref"], "#/$defs/step");
        assert_eq!(defs["loop"]["properties"]["steps"]["$ref"], "#/$defs/steps");
        assert_eq!(
            defs["loop"]["properties"]["until"]["$ref"],
            "#/$defs/reference"
        );
        assert!(defs["loop"]["properties"]["until"]["description"]
            .as_str()
            .unwrap()
            .contains("boolean"));
        let branches = &defs["parallel"]["properties"]["branches"];
        assert_eq!(branches["type"], "object");
        assert_eq!(branches["minProperties"], 1);
        assert_eq!(branches["additionalProperties"], false);
        assert_eq!(
            branches["patternProperties"][NAME_PATTERN]["$ref"],
            "#/$defs/operation"
        );
        let over = &defs["fan_out"]["properties"]["over"];
        assert_eq!(
            over["oneOf"],
            json!([
                {"type": "array", "items": {}}, {"$ref": "#/$defs/reference"}
            ])
        );
        for (definition, field, default) in [
            ("fan_out", "repeat", 1),
            ("loop", "max", 5),
            ("loop_shape", "max_rounds", 5),
        ] {
            let count = &defs[definition]["properties"][field];
            assert_eq!(count["type"], "integer");
            assert_eq!(count["minimum"], 1);
            assert_eq!(count["maximum"], 20);
            assert_eq!(count["default"], default);
        }
        assert!(defs["fan_out"]["properties"]["repeat"]["description"]
            .as_str()
            .unwrap()
            .contains("duplicate reviewers"));
        assert_eq!(defs["output_schema"]["type"], "object");
        assert!(defs["output_schema"].get("properties").is_none());
        assert!(defs["output_schema"].get("additionalProperties").is_none());
        for (definition, field) in [
            ("agent", "schema"),
            ("fan_out", "schema"),
            ("fan_out_shape", "map_schema"),
            ("map_reduce_shape", "map_schema"),
            ("map_reduce_shape", "reduce_schema"),
        ] {
            assert_eq!(
                defs[definition]["properties"][field]["$ref"],
                "#/$defs/output_schema"
            );
        }
    }

    #[test]
    fn every_schema_reference_resolves() {
        fn visit(value: &Value, root: &Value) {
            match value {
                Value::Object(object) => {
                    if let Some(reference) = object.get("$ref") {
                        let reference = reference.as_str().unwrap();
                        let pointer = reference.strip_prefix('#').unwrap();
                        assert!(root.pointer(pointer).is_some(), "unresolved: {reference}");
                    }
                    for value in object.values() {
                        visit(value, root);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        visit(value, root);
                    }
                }
                _ => {}
            }
        }
        let schema = schema();
        visit(&schema, &schema);
    }
}
