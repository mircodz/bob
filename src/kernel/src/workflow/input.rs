use serde_json::{json, Map, Value};

const NAME_PATTERN: &str = "^[A-Za-z][A-Za-z0-9_-]*$";
const REFERENCE_PATTERN: &str = r"^\$[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z0-9_-]+)*$";
const OPERATIONS: [&str; 5] = ["agent", "fan_out", "parallel", "loop", "conditional"];
const MAX_COUNT: u64 = 20;
const REDUCE_DEFAULT: &str = "Synthesize these results into a single summary.";
const FIND_DEFAULT: &str =
    "Find items not already in the seen list; return {findings:[..]}, empty if none new.";

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
            "description": "A JSON Schema object for agent output. Supported JSON Schema drafts are fully validated; external references and unknown dialects are rejected."
        },
        "items": {
            "type": "array",
            "minItems": 1,
            "maxItems": super::validation::MAX_ITEMS,
            "items": {"type": "string"}
        },
        "steps": {
            "type": "array",
            "minItems": 1,
            "maxItems": super::validation::MAX_ITEMS,
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
                    {"type": "array", "maxItems": super::validation::MAX_ITEMS, "items": {}},
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
    definitions["conditional"] = object_schema(
        json!({
            "if": {
                "$ref": "#/$defs/reference",
                "description": "Reference resolving to an actual boolean. The chosen branch is retained before children are admitted and is not reevaluated."
            },
            "then": {"$ref": "#/$defs/steps"},
            "else": {
                "$ref": "#/$defs/steps",
                "description": "Optional false branch; omission is a no-op. Output is {branch: then|else, output: {chosen step results}}."
            }
        }),
        &["if", "then"],
    );
    definitions["step"] = operation_schema(true);
    definitions["operation"] = operation_schema(false);

    let common = json!({
        "title": {"type": "string", "description": "Short label for the workflow run."},
        "max_concurrency": {
            "type": "integer", "minimum": 1, "maximum": 64, "default": 64,
            "description": "One workflow-wide active-child budget across nested operations, still subject to global/session limits. Waiting for guidance and unsettled stopping children occupy slots. Does not isolate shared-workspace edits."
        },
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

/// Checks structure, not reference visibility; output schemas reject external references.
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
    if let Some(count) = object.get("max_concurrency") {
        if !count
            .as_u64()
            .is_some_and(|count| (1..=64).contains(&count))
        {
            return Err("$.max_concurrency must be an integer in 1..=64".into());
        }
    }
    match (object.contains_key("steps"), object.contains_key("shape")) {
        (true, false) => {
            fields(
                object,
                &["title", "read_only", "max_concurrency", "steps"],
                "$",
            )?;
            validate_steps(&object["steps"], "$.steps", 0)
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
            "max_concurrency",
            "shape",
            "items",
            "map_prompt",
            "map_schema",
        ],
        "map_reduce" => &[
            "title",
            "read_only",
            "max_concurrency",
            "shape",
            "items",
            "map_prompt",
            "map_schema",
            "reduce_prompt",
            "reduce_schema",
        ],
        "loop" => &[
            "title",
            "read_only",
            "max_concurrency",
            "shape",
            "find_prompt",
            "max_rounds",
        ],
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

fn validate_steps(value: &Value, path: &str, depth: usize) -> Result<(), String> {
    for (index, step) in nonempty_array(value, path)?.iter().enumerate() {
        validate_operation(step, &format!("{path}[{index}]"), true, depth)?;
    }
    Ok(())
}

fn validate_operation(value: &Value, path: &str, named: bool, depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("workflow nesting exceeds 16 levels".into());
    }
    let object = as_object(value, path)?;
    if named {
        fields(
            object,
            &["id", "agent", "fan_out", "parallel", "loop", "conditional"],
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
            "{path} must contain exactly one operation: agent, fan_out, parallel, loop or conditional"
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
            let prompt_value = required(body, "prompt", &path)?;
            prompt(prompt_value, &format!("{path}.prompt"))?;
            if let Some(items) = over.as_array() {
                super::validation::validate_items(prompt_value.as_str().unwrap(), items)
                    .map_err(|error| format!("{path}: {error}"))?;
            }
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
                validate_operation(branch, &format!("{path}.{name}"), false, depth + 1)?;
            }
        }
        "loop" => {
            fields(body, &["steps", "until", "max"], &path)?;
            validate_steps(
                required(body, "steps", &path)?,
                &format!("{path}.steps"),
                depth + 1,
            )?;
            if let Some(until) = body.get("until") {
                reference(until, &format!("{path}.until"))?;
            }
            optional_count(body, "max", &path)?;
        }
        "conditional" => {
            fields(body, &["if", "then", "else"], &path)?;
            reference(required(body, "if", &path)?, &format!("{path}.if"))?;
            validate_steps(
                required(body, "then", &path)?,
                &format!("{path}.then"),
                depth + 1,
            )?;
            if let Some(otherwise) = body.get("else") {
                validate_steps(otherwise, &format!("{path}.else"), depth + 1)?;
            }
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
    if array.len() > super::validation::MAX_ITEMS {
        return Err(format!(
            "{path} exceeds {} items",
            super::validation::MAX_ITEMS
        ));
    }
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
        super::schema::CompiledSchema::compile(value)?;
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
