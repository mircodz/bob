use super::dsl::{Op, Spec, Step};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

#[derive(Clone)]
enum Shape {
    Dynamic,
    Object(BTreeMap<String, Shape>),
    Array,
    Scalar,
}

type Scope = BTreeMap<String, Shape>;

pub(crate) fn validate(spec: &Spec) -> Result<(), String> {
    validate_steps(&spec.steps, &Scope::new(), &BTreeSet::new()).map(|_| ())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn check(steps: Value) -> Result<(), String> {
        let spec: Spec = serde_json::from_value(json!({ "steps": steps })).unwrap();
        validate(&spec)
    }

    fn agent(id: &str, prompt: &str) -> Value {
        json!({ "id": id, "agent": { "prompt": prompt } })
    }

    fn rejects(steps: Value, expected: &str) {
        let error = check(steps).unwrap_err();
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
    }

    #[test]
    fn validates_nested_loops_parallel_branches_and_items() {
        check(json!([
            agent("seed", "produce input"),
            { "id": "rounds", "loop": {
                "steps": [
                    { "id": "checks", "parallel": { "branches": {
                        "left": { "loop": {
                            "steps": [agent("draft", "read {$seed.payload.0}")],
                            "until": "$draft.done"
                        } },
                        "right": { "fan_out": {
                            "over": "$seed.list",
                            "prompt": "{item} {item.details.0.name} {$seed.context}"
                        } }
                    } } },
                    agent("verdict", "{$checks.left.draft.done} {$checks.right}")
                ],
                "until": "$verdict.done"
            } },
            agent("summary", concat!(
                "{$rounds.checks.left.draft.any_field} ",
                "{$rounds.checks.left._iterations} ",
                "{$rounds._iterations} {$rounds._stop_reason}"
            )),
            { "id": "finish", "fan_out": {
                "over": "$rounds.checks.right",
                "prompt": "{\"result\": {\"value\": \"{item.details.name}\"}, \"meta\": \"{$summary}\"}"
            } }
        ]))
        .unwrap();
    }

    #[test]
    fn leaves_agent_output_fields_and_schema_contents_to_runtime() {
        check(json!([
            { "id": "source", "agent": {
                "prompt": "produce output",
                "schema": {
                    "type": "object",
                    "properties": { "declared": { "type": "string" } },
                    "additionalProperties": true,
                    "description": "{$not_a_prompt} {item}"
                }
            } },
            agent("use", "{$source.undeclared.nested.0._field.a-b}")
        ]))
        .unwrap();
    }

    #[test]
    fn rejects_unknown_forward_and_self_references() {
        for reference in ["$missing", "$later.value", "$first"] {
            rejects(
                json!([
                    agent("first", &format!("read {{{reference}}}")),
                    agent("later", "produce output")
                ]),
                "unavailable result",
            );
        }
        rejects(
            json!([{ "id": "fan", "fan_out": {
                "over": "$missing.items", "prompt": "{item}"
            } }]),
            "unavailable result",
        );
        rejects(
            json!([{ "id": "fan", "fan_out": {
                "over": [], "prompt": "{item} {$missing}"
            } }]),
            "unavailable result",
        );
    }

    #[test]
    fn rejects_duplicate_and_shadowed_names() {
        rejects(json!([agent("same", "a"), agent("same", "b")]), "duplicate");
        for inner in ["outer", "container", "later"] {
            rejects(
                json!([
                    agent("outer", "a"),
                    { "id": "container", "loop": { "steps": [agent(inner, "b")] } },
                    agent("later", "c")
                ]),
                "shadowed",
            );
        }
        rejects(
            json!([{ "id": "container", "loop": {
                "steps": [agent("inner", "a"), agent("inner", "b")]
            } }]),
            "duplicate",
        );
        for branch in ["outer", "container"] {
            let branches = BTreeMap::from([(branch, json!({ "agent": { "prompt": "b" } }))]);
            rejects(
                json!([
                    agent("outer", "a"),
                    { "id": "container", "parallel": { "branches": branches } }
                ]),
                "shadowed",
            );
        }
        rejects(
            json!([{ "id": "container", "parallel": { "branches": {
                "branch": { "loop": { "steps": [agent("branch", "a")] } }
            } } }]),
            "shadowed",
        );
    }

    #[test]
    fn permits_reusing_local_names_in_disjoint_scopes() {
        check(json!([
            { "id": "first", "loop": { "steps": [agent("local", "a")] } },
            { "id": "second", "loop": { "steps": [agent("local", "b")] } },
            { "id": "branches", "parallel": { "branches": {
                "left": { "loop": { "steps": [agent("local", "c")] } },
                "right": { "loop": { "steps": [agent("local", "d")] } }
            } } },
            agent("last", "{$first.local} {$second.local} {$branches.left.local}")
        ]))
        .unwrap();
    }

    #[test]
    fn rejects_invalid_names_and_empty_containers() {
        for name in [
            "",
            "_iterations",
            "_stop_reason",
            "1step",
            "a.b",
            "a b",
            "a$",
            "é",
        ] {
            rejects(json!([agent(name, "a")]), "invalid step or branch name");
            let branches = BTreeMap::from([(name, json!({ "agent": { "prompt": "b" } }))]);
            rejects(
                json!([{ "id": "group", "parallel": { "branches": branches } }]),
                "invalid step or branch name",
            );
        }
        check(json!([agent("A0_b-c", "valid")])).unwrap();
        rejects(json!([]), "steps must not be empty");
        rejects(
            json!([{ "id": "rounds", "loop": { "steps": [] } }]),
            "steps must not be empty",
        );
        rejects(
            json!([{ "id": "group", "parallel": { "branches": {} } }]),
            "branches must not be empty",
        );
    }

    #[test]
    fn rejects_loop_local_leakage_and_previous_iteration_dependencies() {
        rejects(
            json!([
                { "id": "rounds", "loop": { "steps": [agent("local", "a")] } },
                agent("after", "{$local}")
            ]),
            "unavailable result",
        );
        for reference in ["$last", "$first", "$rounds.last"] {
            rejects(
                json!([{ "id": "rounds", "loop": {
                    "steps": [agent("first", &format!("{{{reference}}}")), agent("last", "a")],
                    "max": 2
                } }]),
                "unavailable result",
            );
        }
        rejects(
            json!([{ "id": "rounds", "loop": {
                "steps": [
                    { "id": "nested", "loop": { "steps": [agent("local", "a")] } }
                ],
                "until": "$local.done"
            } }]),
            "unavailable result",
        );
    }

    #[test]
    fn rejects_parallel_sibling_self_and_local_leakage() {
        for reference in ["$left", "$right", "$group.left", "$local"] {
            rejects(
                json!([{ "id": "group", "parallel": { "branches": {
                    "left": { "loop": { "steps": [agent("local", "a")] } },
                    "right": { "agent": { "prompt": format!("{{{reference}}}") } }
                } } }]),
                "unavailable result",
            );
        }
        rejects(
            json!([
                { "id": "group", "parallel": { "branches": {
                    "branch": { "agent": { "prompt": "a" } }
                } } },
                agent("after", "{$branch}")
            ]),
            "unavailable result",
        );
    }

    #[test]
    fn checks_until_against_outer_and_all_inner_results() {
        for until in ["$outer.done", "$last.done", "$nested.local.done"] {
            check(json!([
                agent("outer", "a"),
                { "id": "rounds", "loop": {
                    "steps": [
                        agent("first", "{$outer}"),
                        { "id": "nested", "loop": { "steps": [agent("local", "{$first}")] } },
                        agent("last", "{$nested.local}")
                    ],
                    "until": until
                } }
            ]))
            .unwrap();
        }
        for until in ["$missing", "$later.done", "$rounds._iterations"] {
            rejects(
                json!([
                    { "id": "rounds", "loop": {
                        "steps": [agent("inner", "a")], "until": until
                    } },
                    agent("later", "b")
                ]),
                "unavailable result",
            );
        }
    }

    #[test]
    fn checks_known_containers_but_not_dynamic_fields() {
        for reference in [
            "$rounds.missing",
            "$rounds.outer",
            "$rounds.group.missing",
            "$rounds.group.batch.field",
            "$rounds.group.batch.0",
            "$rounds._iterations.field",
            "$rounds._stop_reason.field",
        ] {
            rejects(
                json!([
                    agent("outer", "a"),
                    { "id": "rounds", "loop": { "steps": [
                        { "id": "group", "parallel": { "branches": {
                            "batch": { "fan_out": { "over": [], "prompt": "{item}" } },
                            "dynamic": { "agent": { "prompt": "a" } }
                        } } }
                    ] } },
                    agent("after", &format!("{{{reference}}}"))
                ]),
                if reference.contains("batch") {
                    "array"
                } else if reference.contains("._") {
                    "scalar"
                } else {
                    "unknown object field"
                },
            );
        }
        rejects(
            json!([{ "id": "rounds", "loop": {
                "steps": [{ "id": "group", "parallel": { "branches": {
                    "branch": { "agent": { "prompt": "a" } }
                } } }],
                "until": "$group.missing"
            } }]),
            "unknown object field",
        );
        rejects(
            json!([
                { "id": "batch", "fan_out": { "over": [], "prompt": "{item}" } },
                { "id": "next", "fan_out": { "over": "$batch.0", "prompt": "{item}" } }
            ]),
            "array",
        );
    }

    #[test]
    fn reference_grammar_allows_numeric_object_fields_only() {
        assert_eq!(
            reference_parts("$Step-1_a.0._iterations.a-b").unwrap(),
            ["Step-1_a", "0", "_iterations", "a-b"]
        );
        for reference in [
            "",
            "$",
            "step",
            "$$step",
            "$1step",
            "$_step",
            "$step.",
            "$step..field",
            "$step.a b",
            " $step",
            "$step\n",
            "$step[0]",
            "$step.é",
            "${step}",
            "{$step}",
            "$step.$field",
            "$step./field",
        ] {
            assert!(reference_parts(reference).is_err(), "{reference:?}");
        }
    }

    #[test]
    fn scanner_finds_reserved_placeholders_inside_literal_json() {
        let template = r#"é {"literal": {"nested": 1}, "ref": "{$step.0}", "item": "{item.field.nested}"} {unknown} {items} {item_name} {item-name} {{wrapper: {$other}}} {item}"#;
        let found = placeholders(template).unwrap();
        assert_eq!(
            found.iter().map(|(_, key)| *key).collect::<Vec<_>>(),
            ["$step.0", "item.field.nested", "$other", "item"]
        );
        for (range, key) in &found {
            assert_eq!(&template[range.clone()], format!("{{{key}}}"));
        }
        assert!(found
            .windows(2)
            .all(|pair| pair[0].0.end <= pair[1].0.start));
        let literal = r#"{"x": {"y": 1}} {unknown} {items} trailing {"#;
        assert!(placeholders(literal).unwrap().is_empty());
        check(json!([
            agent("step", "a"),
            agent("after", r#"{"literal": {}, "ref": "{$step.0}"} {unknown}"#)
        ]))
        .unwrap();
        rejects(
            json!([agent("first", r#"{"value": "{$missing}"}"#)]),
            "unavailable result",
        );
    }

    #[test]
    fn rejects_malformed_recognized_placeholders() {
        for template in [
            "{$}",
            "{$step.}",
            "{$step..field}",
            "{$step field}",
            "{$step[0]}",
            "{item.}",
            "{item..field}",
            "{item.field.}",
            "{item.field name}",
            "{item[0]}",
            "{item.$field}",
            "{item.é}",
            "{$step{nested}}",
        ] {
            assert!(placeholders(template).is_err(), "{template:?}");
        }
        for reference in ["$source.", "$source..field", "$source[0]"] {
            rejects(
                json!([
                    agent("source", "a"),
                    { "id": "fan", "fan_out": { "over": reference, "prompt": "{item}" } }
                ]),
                "invalid reference",
            );
            rejects(
                json!([{ "id": "rounds", "loop": {
                    "steps": [agent("source", "a")], "until": reference
                } }]),
                "invalid reference",
            );
        }
    }

    #[test]
    fn rejects_items_outside_fan_out_prompts() {
        for prompt in ["{item}", "{item.field.nested}", r#"{"value": "{item}"}"#] {
            rejects(
                json!([agent("plain", prompt)]),
                "only allowed in a fan_out prompt",
            );
            rejects(
                json!([{ "id": "group", "parallel": { "branches": {
                    "branch": { "agent": { "prompt": prompt } }
                } } }]),
                "only allowed in a fan_out prompt",
            );
        }
    }

    #[test]
    fn rejects_unterminated_reserved_placeholders_only() {
        for template in [
            "{$",
            "prefix {$step",
            "{item",
            "{item.field",
            r#"{"x": "{$step"#,
        ] {
            let error = placeholders(template).unwrap_err();
            assert!(error.contains("unterminated"), "{template:?}: {error}");
        }
        for template in ["{", "{unknown", "{items", r#"{"literal": {"#] {
            assert!(placeholders(template).unwrap().is_empty(), "{template:?}");
        }
    }
}
