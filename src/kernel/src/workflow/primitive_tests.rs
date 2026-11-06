use super::{dsl, input, params, schema::CompiledSchema, validation};
use serde_json::{json, Value};

fn pipeline(conditional: Value) -> Value {
    json!({"steps":[
        {"id":"check","agent":{"prompt":"Check","schema":{"type":"object"}}},
        {"id":"decision","conditional":conditional}
    ]})
}

fn validate(value: &Value) -> Result<(), String> {
    input::validate(value)?;
    if value.get("steps").is_some() {
        validation::validate(&serde_json::from_value(value.clone()).unwrap())
    } else {
        params::validate(&serde_json::from_value(value.clone()).unwrap())
    }
}

#[test]
fn concurrency_serializers_omit_default_budget_and_preserve_explicit_values() {
    for count in [None, Some(1), Some(64)] {
        let mut pipeline = json!({"steps":[{"id":"work","agent":{"prompt":"Work"}}]});
        let mut shape = json!({"shape":"fan_out","items":["one"],"map_prompt":"{item}"});
        if let Some(count) = count {
            pipeline["max_concurrency"] = json!(count);
            shape["max_concurrency"] = json!(count);
        }
        let spec: dsl::Spec = serde_json::from_value(pipeline).unwrap();
        let params: params::WorkflowParams = serde_json::from_value(shape).unwrap();
        for serialized in [
            serde_json::to_value(spec).unwrap(),
            serde_json::to_value(params).unwrap(),
        ] {
            assert_eq!(
                serialized.get("max_concurrency"),
                count.map(|count| json!(count)).as_ref()
            );
        }
    }
}

#[test]
fn concurrency_schema_and_runtime_validation_agree_on_all_forms() {
    let schema = CompiledSchema::compile(&input::schema()).unwrap();
    let forms = [
        json!({"steps":[{"id":"work","agent":{"prompt":"Work"}}]}),
        json!({"shape":"fan_out","items":["one"],"map_prompt":"{item}"}),
        json!({"shape":"map_reduce","items":["one"],"map_prompt":"{item}"}),
        json!({"shape":"loop"}),
    ];
    for mut form in forms {
        assert!(validate(&form).is_ok());
        assert!(schema.validate(&form).is_ok());
        for count in [json!(1), json!(2), json!(64)] {
            form["max_concurrency"] = count;
            assert!(validate(&form).is_ok());
            assert!(schema.validate(&form).is_ok());
        }
        for count in [
            json!(0),
            json!(65),
            json!(-1),
            json!(1.5),
            json!("2"),
            Value::Null,
        ] {
            form["max_concurrency"] = count;
            assert!(validate(&form).is_err(), "{form}");
            assert!(schema.validate(&form).is_err(), "{form}");
        }
    }
}

#[test]
fn conditional_schema_requires_reference_and_nonempty_branch_sequences() {
    let schema = CompiledSchema::compile(&input::schema()).unwrap();
    let good = json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"Work"}}]});
    assert!(validate(&pipeline(good.clone())).is_ok());
    assert!(schema.validate(&pipeline(good.clone())).is_ok());
    let mut both = good.clone();
    both["else"] = json!([{"id":"work","agent":{"prompt":"Other work"}}]);
    assert!(validate(&pipeline(both.clone())).is_ok());
    assert!(schema.validate(&pipeline(both)).is_ok());
    for (field, invalid) in [
        ("if", json!(true)),
        ("if", json!("$check.ok == true")),
        ("then", json!([])),
        ("else", json!([])),
        ("else", Value::Null),
        ("max_concurrency", json!(1)),
    ] {
        let mut bad = good.clone();
        bad[field] = invalid;
        assert!(validate(&pipeline(bad.clone())).is_err());
        assert!(schema.validate(&pipeline(bad)).is_err());
    }
    let mut missing = good;
    missing.as_object_mut().unwrap().remove("then");
    assert!(validate(&pipeline(missing.clone())).is_err());
    assert!(schema.validate(&pipeline(missing)).is_err());
}

#[test]
fn conditional_checks_both_branch_scopes_without_leaking_outputs() {
    for conditional in [
        json!({"if":"$future.ok","then":[{"id":"work","agent":{"prompt":"Work"}}]}),
        json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"Work"}}],"else":[{"id":"other","agent":{"prompt":"{$work}"}}]}),
        json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"{$later}"}},{"id":"later","agent":{"prompt":"Later"}}]}),
        json!({"if":"$check.ok","then":[{"id":"check","agent":{"prompt":"Shadowed"}}]}),
        json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"Work"}}],"else":[{"id":"other","agent":{"prompt":"{$missing}"}}]}),
    ] {
        assert!(validate(&pipeline(conditional)).is_err());
    }
    let mut value =
        pipeline(json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"{$check}"}}]}));
    value["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"after","agent":{"prompt":"{$decision.output.work}"}}));
    assert!(validate(&value).is_ok());
    for reference in [
        "{$work}",
        "{$decision.work}",
        "{$decision.output.missing}",
        "{$decision.branch.field}",
    ] {
        value["steps"][2]["agent"]["prompt"] = json!(reference);
        assert!(validate(&value).is_err(), "{reference}");
    }
}

#[test]
fn conditional_static_schema_and_item_errors_in_unselected_branch_are_rejected() {
    for invalid in [
        json!({"agent":{"prompt":"Work","schema":{"type":"not-a-type"}}}),
        json!({"fan_out":{"over":[{}],"prompt":"{item.missing}"}}),
        json!({"agent":{"prompt":" "}}),
    ] {
        let mut step = invalid;
        step["id"] = json!("invalid");
        let value = pipeline(
            json!({"if":"$check.ok","then":[{"id":"work","agent":{"prompt":"Work"}}],"else":[step]}),
        );
        assert!(validate(&value).is_err());
    }
}

#[test]
fn both_conditional_branches_consume_static_operation_budget() {
    let steps = |prefix: &str, count: usize| {
        (0..count)
            .map(|i| json!({"id":format!("{prefix}{i}"),"agent":{"prompt":"Work"}}))
            .collect::<Vec<_>>()
    };
    let mut value =
        pipeline(json!({"if":"$check.ok","then":steps("yes", 127),"else":steps("no", 127)}));
    assert!(validate(&value).is_ok());
    value["steps"][1]["conditional"]["else"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"extra","agent":{"prompt":"Work"}}));
    let error = validate(&value).unwrap_err();
    assert!(error.contains("256 operations"), "{error}");
}

#[test]
fn unselected_conditional_branch_consumes_nesting_budget() {
    let mut nested = json!({"id":"leaf","agent":{"prompt":"Work"}});
    for i in 0..18 {
        nested = json!({"id":format!("level{i}"),"conditional":{"if":"$check.ok","then":[{"id":format!("yes{i}"),"agent":{"prompt":"Work"}}],"else":[nested]}});
    }
    let value = json!({"steps":[{"id":"check","agent":{"prompt":"Check"}},nested]});
    assert!(input::validate(&value).is_err());
    let spec: dsl::Spec = serde_json::from_value(value).unwrap();
    assert!(validation::validate(&spec).unwrap_err().contains("nesting"));
}

#[test]
fn preflight_defers_conditional_inputs_until_durable_selection() {
    let op: dsl::Op = serde_json::from_value(json!({"conditional":{
        "if":"$check.missing",
        "then":[{"id":"work","fan_out":{"over":"$check.items","prompt":"{item}"}}],
        "else":[{"id":"other","agent":{"prompt":"{$check.absent}"}}]
    }}))
    .unwrap();
    let scope = json!({"check":{"items":"not-an-array"}})
        .as_object()
        .unwrap()
        .clone();
    assert!(dsl::preflight(&op, &scope).is_ok());
}

#[test]
fn all_model_guidance_examples_validate_and_do_not_force_read_only_or_schemas() {
    let schema = CompiledSchema::compile(&input::schema()).unwrap();
    let mut count = 0;
    for line in super::GUIDANCE.lines().filter(|line| line.starts_with('{')) {
        let value: Value = serde_json::from_str(line).unwrap();
        validate(&value).unwrap();
        schema.validate(&value).unwrap();
        count += 1;
    }
    assert_eq!(count, 4);
    validate(&json!({"max_concurrency":1,"steps":[{"id":"edit","agent":{"prompt":"Edit the assigned file"}}]})).unwrap();
}
