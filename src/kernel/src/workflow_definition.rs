use crate::WorkflowNodeKind;
use serde_json::{json, Value};

#[derive(Clone)]
pub(super) struct Declared {
    pub path: String,
    pub specification: Value,
    pub kind: WorkflowNodeKind,
    pub dependency: Option<String>,
}

fn kind(spec: &Value) -> WorkflowNodeKind {
    if spec.get("agent").is_some() || spec.get("prompt").is_some() {
        WorkflowNodeKind::Agent
    } else if spec.get("fan_out").is_some() {
        WorkflowNodeKind::FanOut
    } else if spec.get("parallel").is_some() {
        WorkflowNodeKind::Parallel
    } else if spec.get("loop").is_some() {
        WorkflowNodeKind::Loop
    } else if spec.get("conditional").is_some() {
        WorkflowNodeKind::Conditional
    } else if spec.get("steps").is_some() {
        WorkflowNodeKind::Branch
    } else {
        WorkflowNodeKind::Unknown
    }
}

pub(super) fn from_spec(path: String, specification: Value) -> Declared {
    Declared {
        path,
        kind: kind(&specification),
        specification,
        dependency: None,
    }
}

fn steps(prefix: &str, values: &[Value]) -> Vec<Declared> {
    let mut previous = None;
    values
        .iter()
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?;
            let path = format!("{prefix}/{id}");
            let mut specification = value.clone();
            specification.as_object_mut()?.remove("id");
            let mut declared = from_spec(path.clone(), specification);
            declared.dependency = previous.replace(path);
            Some(declared)
        })
        .collect()
}

pub(super) fn children(definition: &Value, container: Option<&str>) -> Vec<Declared> {
    let Some(container) = container else {
        if let Some(values) = definition.get("steps").and_then(Value::as_array) {
            return steps("root", values);
        }
        return match definition.get("shape").and_then(Value::as_str) {
            Some("loop") => vec![from_spec(
                "find".into(),
                json!({"loop":{"steps":[],"max":definition["max_rounds"]}}),
            )],
            Some("fan_out" | "map_reduce") => {
                let mut out = vec![from_spec(
                    "map".into(),
                    json!({"fan_out":{"over":definition["items"],"prompt":definition["map_prompt"]}}),
                )];
                if definition["shape"] == "map_reduce" {
                    let mut reduce = from_spec(
                        "reduce".into(),
                        json!({"agent":{"prompt":definition["reduce_prompt"]}}),
                    );
                    reduce.dependency = Some("map".into());
                    out.push(reduce);
                }
                out
            }
            _ => Vec::new(),
        };
    };
    let Some(declared) = find(definition, container) else {
        return Vec::new();
    };
    let specification = &declared.specification;
    if let Some(values) = specification.get("steps").and_then(Value::as_array) {
        return steps(container, values);
    }
    if let Some(branches) = specification
        .pointer("/parallel/branches")
        .and_then(Value::as_object)
    {
        return branches
            .iter()
            .map(|(name, spec)| from_spec(format!("{container}/{name}"), spec.clone()))
            .collect();
    }
    if let Some(conditional) = specification.get("conditional") {
        return ["then", "else"]
            .into_iter()
            .map(|branch| {
                from_spec(
                    format!("{container}/{branch}"),
                    json!({"steps":conditional.get(branch).cloned().unwrap_or_else(||json!([]))}),
                )
            })
            .collect();
    }
    Vec::new()
}

pub(super) fn find(definition: &Value, path: &str) -> Option<Declared> {
    let parts: Vec<_> = path.split('/').collect();
    let (mut current, mut consumed) = if parts.first() == Some(&"root") {
        let id = parts.get(1)?;
        let roots = steps("root", definition.get("steps")?.as_array()?);
        (
            roots
                .into_iter()
                .find(|node| node.path == format!("root/{id}"))?,
            2,
        )
    } else {
        let root = children(definition, None)
            .into_iter()
            .find(|node| node.path == parts[0])?;
        (root, 1)
    };
    while consumed < parts.len() {
        let segment = parts[consumed];
        let prefix = parts[..=consumed].join("/");
        if let Some(values) = current.specification.get("steps").and_then(Value::as_array) {
            current = steps(&current.path, values)
                .into_iter()
                .find(|node| node.path == prefix)?;
        } else if let Some(branches) = current
            .specification
            .pointer("/parallel/branches")
            .and_then(Value::as_object)
        {
            current = from_spec(prefix, branches.get(segment)?.clone());
        } else if let Some(conditional) = current.specification.get("conditional") {
            if !matches!(segment, "then" | "else") {
                return None;
            }
            current = from_spec(
                prefix,
                json!({"steps": conditional.get(segment).cloned().unwrap_or_else(||json!([]))}),
            );
        } else {
            let loop_op = current.specification.get("loop")?;
            segment.strip_prefix("pass")?.parse::<usize>().ok()?;
            if current.path == "find" && definition["shape"] == "loop" {
                current = from_spec(
                    prefix,
                    json!({"agent":{"prompt":definition["find_prompt"]}}),
                );
            } else {
                current = from_spec(prefix, json!({"steps":loop_op["steps"]}));
                current.kind = WorkflowNodeKind::Iteration;
            }
        }
        consumed += 1;
    }
    Some(current)
}
