use super::dsl::{Op, Spec, Step};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

#[derive(Clone, PartialEq, Eq)]
enum Shape {
    Dynamic,
    Object(BTreeMap<String, Shape>),
    Array,
    Scalar,
}

type Scope = BTreeMap<String, Shape>;

pub(crate) const MAX_ITEMS: usize = 256;
const MAX_OPERATIONS: usize = 256;
const MAX_NESTING: usize = 16;

pub(crate) fn validate(spec: &Spec) -> Result<(), String> {
    validate_concurrency(spec.max_concurrency)?;
    let mut remaining = MAX_OPERATIONS;
    validate_limits(&spec.steps, 0, &mut remaining)?;
    validate_steps(&spec.steps, &Scope::new(), &BTreeSet::new()).map(|_| ())
}

fn validate_limits(steps: &[Step], depth: usize, remaining: &mut usize) -> Result<(), String> {
    for step in steps {
        validate_op_limits(&step.op, depth, remaining)?;
    }
    Ok(())
}

fn validate_op_limits(op: &Op, depth: usize, remaining: &mut usize) -> Result<(), String> {
    if depth > MAX_NESTING || *remaining == 0 {
        return Err(format!(
            "workflow exceeds {MAX_NESTING} nesting levels or {MAX_OPERATIONS} operations"
        ));
    }
    *remaining -= 1;
    match op {
        Op::Agent(agent) => validate_agent(&agent.prompt, agent.schema.as_ref()),
        Op::FanOut(fan) => {
            validate_agent(&fan.prompt, fan.schema.as_ref())?;
            validate_count(fan.repeat.unwrap_or(1))?;
            match &fan.over {
                serde_json::Value::Array(items) => validate_items(&fan.prompt, items),
                serde_json::Value::String(_) => Ok(()),
                _ => Err(format!(
                    "fan-out over must be a reference or at most {MAX_ITEMS} items"
                )),
            }
        }
        Op::Parallel(parallel) => {
            for op in parallel.branches.values() {
                validate_op_limits(op, depth + 1, remaining)?;
            }
            Ok(())
        }
        Op::Loop(loop_op) => {
            validate_count(loop_op.max.unwrap_or(5))?;
            validate_limits(&loop_op.steps, depth + 1, remaining)
        }
        Op::Conditional(conditional) => {
            validate_limits(&conditional.then, depth + 1, remaining)?;
            if let Some(otherwise) = &conditional.otherwise {
                validate_limits(otherwise, depth + 1, remaining)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn item_value<'a>(
    key: &str,
    item: &'a serde_json::Value,
) -> Result<&'a serde_json::Value, String> {
    let mut value = item;
    for field in key.split('.').skip(1) {
        value = value
            .get(field)
            .ok_or_else(|| format!("missing item field in '{{{key}}}'"))?;
    }
    Ok(value)
}

pub(crate) fn validate_items(prompt: &str, items: &[serde_json::Value]) -> Result<(), String> {
    if items.len() > MAX_ITEMS {
        return Err(format!("fan-out exceeds {MAX_ITEMS} items"));
    }
    let placeholders = placeholders(prompt)?;
    for (index, item) in items.iter().enumerate() {
        for (_, key) in &placeholders {
            if !key.starts_with('$') {
                item_value(key, item)
                    .map_err(|error| format!("fan-out item {}: {error}", index + 1))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_concurrency(count: Option<usize>) -> Result<(), String> {
    if !(1..=64).contains(&count.unwrap_or(64)) {
        return Err("max_concurrency must be in 1..=64".into());
    }
    Ok(())
}

pub(crate) fn validate_count(count: usize) -> Result<(), String> {
    if !(1..=20).contains(&count) {
        return Err("workflow count must be in 1..=20".into());
    }
    Ok(())
}

pub(crate) fn validate_agent(
    prompt: &str,
    schema: Option<&serde_json::Value>,
) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("agent prompt must not be blank".into());
    }
    if let Some(schema) = schema {
        super::schema::CompiledSchema::compile(schema)?;
    }
    Ok(())
}

pub(crate) fn reference_parts(reference: &str) -> Result<Vec<&str>, String> {
    let invalid = || format!("invalid reference {reference:?}");
    let path = reference.strip_prefix('$').ok_or_else(invalid)?;
    let parts: Vec<_> = path.split('.').collect();
    if !valid_name(parts[0]) || parts[1..].iter().any(|part| !valid_field(part)) {
        return Err(invalid());
    }
    Ok(parts)
}

pub(crate) fn placeholders(template: &str) -> Result<Vec<(Range<usize>, &str)>, String> {
    let mut found = Vec::new();
    let mut offset = 0;
    while let Some(relative) = template[offset..].find('{') {
        let open = offset + relative;
        let start = open + 1;
        let after = &template[start..];
        let item = after.strip_prefix("item").is_some_and(|suffix| {
            suffix
                .as_bytes()
                .first()
                .is_none_or(|byte| !field_byte(*byte))
        });
        if !after.starts_with('$') && !item {
            offset = start;
            continue;
        }
        let close = after
            .find('}')
            .ok_or_else(|| format!("unterminated reserved placeholder at byte {open}"))?;
        let key = &after[..close];
        if key.starts_with('$') {
            reference_parts(key)?;
        } else if key != "item" {
            let valid = key
                .strip_prefix("item.")
                .is_some_and(|path| path.split('.').all(valid_field));
            if !valid {
                return Err(format!("invalid item placeholder {key:?}"));
            }
        }
        offset = start + close + 1;
        found.push((open..offset, key));
    }
    Ok(found)
}

fn field_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn valid_field(field: &str) -> bool {
    !field.is_empty() && field.bytes().all(field_byte)
}

fn valid_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && valid_field(name)
}

fn declare(name: &str, names: &mut BTreeSet<String>) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("invalid step or branch name {name:?}"));
    }
    if !names.insert(name.to_owned()) {
        return Err(format!(
            "duplicate or shadowed step or branch name {name:?}"
        ));
    }
    Ok(())
}

fn validate_steps(
    steps: &[Step],
    outer: &Scope,
    inherited_names: &BTreeSet<String>,
) -> Result<Scope, String> {
    if steps.is_empty() {
        return Err("steps must not be empty".into());
    }
    // Reserve every declaration, but expose its result only after validating its operation.
    let mut names = inherited_names.clone();
    for step in steps {
        declare(&step.id, &mut names)?;
    }
    let mut scope = outer.clone();
    let mut outputs = Scope::new();
    for step in steps {
        let shape = validate_op(&step.op, &scope, &names)
            .map_err(|error| format!("step {:?}: {error}", step.id))?;
        scope.insert(step.id.clone(), shape.clone());
        outputs.insert(step.id.clone(), shape);
    }
    Ok(outputs)
}

fn validate_op(op: &Op, scope: &Scope, names: &BTreeSet<String>) -> Result<Shape, String> {
    match op {
        Op::Agent(agent) => {
            validate_prompt(&agent.prompt, scope, false)?;
            Ok(Shape::Dynamic)
        }
        Op::FanOut(fan) => {
            if let Some(reference) = fan.over.as_str() {
                validate_reference(reference, scope)?;
            }
            validate_prompt(&fan.prompt, scope, true)?;
            Ok(Shape::Array)
        }
        Op::Parallel(parallel) => {
            if parallel.branches.is_empty() {
                return Err("parallel branches must not be empty".into());
            }
            let mut branch_names = names.clone();
            for name in parallel.branches.keys() {
                declare(name, &mut branch_names)?;
            }
            let mut outputs = Scope::new();
            for (name, branch) in &parallel.branches {
                let shape = validate_op(branch, scope, &branch_names)
                    .map_err(|error| format!("branch {name:?}: {error}"))?;
                outputs.insert(name.clone(), shape);
            }
            Ok(Shape::Object(outputs))
        }
        Op::Conditional(conditional) => {
            validate_reference(&conditional.condition, scope)?;
            let mut outputs = validate_steps(&conditional.then, scope, names)?;
            if let Some(otherwise) = &conditional.otherwise {
                for (name, shape) in validate_steps(otherwise, scope, names)? {
                    outputs
                        .entry(name)
                        .and_modify(|existing| {
                            if *existing != shape {
                                *existing = Shape::Dynamic;
                            }
                        })
                        .or_insert(shape);
                }
            }
            Ok(Shape::Object(BTreeMap::from([
                ("branch".into(), Shape::Scalar),
                ("output".into(), Shape::Object(outputs)),
            ])))
        }
        Op::Loop(loop_op) => {
            let mut outputs = validate_steps(&loop_op.steps, scope, names)?;
            if let Some(until) = &loop_op.until {
                let mut until_scope = scope.clone();
                until_scope.extend(outputs.clone());
                validate_reference(until, &until_scope)?;
            }
            outputs.insert("_iterations".into(), Shape::Scalar);
            outputs.insert("_stop_reason".into(), Shape::Scalar);
            Ok(Shape::Object(outputs))
        }
    }
}

fn validate_prompt(prompt: &str, scope: &Scope, allow_item: bool) -> Result<(), String> {
    for (_, key) in placeholders(prompt)? {
        if key.starts_with('$') {
            validate_reference(key, scope)?;
        } else if !allow_item {
            return Err(format!("{{{key}}} is only allowed in a fan_out prompt"));
        }
    }
    Ok(())
}

fn validate_reference(reference: &str, scope: &Scope) -> Result<(), String> {
    let parts = reference_parts(reference)?;
    let mut shape = scope.get(parts[0]).ok_or_else(|| {
        format!(
            "reference {reference:?} uses unavailable result {:?}",
            parts[0]
        )
    })?;
    for field in &parts[1..] {
        match shape {
            Shape::Dynamic => return Ok(()),
            Shape::Object(fields) => {
                shape = fields.get(*field).ok_or_else(|| {
                    format!("reference {reference:?} has unknown object field {field:?}")
                })?;
            }
            Shape::Array => {
                return Err(format!(
                    "reference {reference:?} cannot address array fields or indices"
                ));
            }
            Shape::Scalar => {
                return Err(format!(
                    "reference {reference:?} cannot address scalar fields"
                ));
            }
        }
    }
    Ok(())
}
