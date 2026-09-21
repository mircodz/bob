use super::PositionEncoding;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) struct DocumentEdits {
    pub version: Option<i64>,
    pub edits: Vec<TextEdit>,
}

pub(crate) struct TextEdit {
    start_line: usize,
    start_char: usize,
    end_line: usize,
    end_char: usize,
    new_text: String,
}

pub(crate) fn collect_workspace_edit(
    result: &Value,
) -> Result<BTreeMap<PathBuf, DocumentEdits>, String> {
    let mut out = BTreeMap::new();
    if result.get("changes").is_some() && result.get("documentChanges").is_some() {
        return Err("workspace edit contains both changes and documentChanges".into());
    }
    if result
        .get("changeAnnotations")
        .and_then(Value::as_object)
        .is_some_and(|annotations| {
            annotations
                .values()
                .any(|a| a["needsConfirmation"].as_bool() == Some(true))
        })
    {
        return Err("workspace edit requires unsupported change-annotation confirmation".into());
    }
    if let Some(changes) = result.get("changes") {
        for (uri, edits) in changes.as_object().ok_or("changes must be an object")? {
            add(&mut out, uri, None, edits)?;
        }
    } else if let Some(changes) = result.get("documentChanges") {
        for entry in changes
            .as_array()
            .ok_or("documentChanges must be an array")?
        {
            if entry.get("kind").is_some() {
                return Err(format!(
                    "unsupported workspace resource operation: {}",
                    entry["kind"]
                ));
            }
            let document = &entry["textDocument"];
            let uri = document["uri"]
                .as_str()
                .ok_or("textDocument.uri is required")?;
            let version = match document.get("version") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_i64().ok_or("invalid document version")?),
            };
            add(&mut out, uri, version, &entry["edits"])?;
        }
    } else {
        return Err("workspace edit had neither changes nor documentChanges".into());
    }
    Ok(out)
}

fn add(
    out: &mut BTreeMap<PathBuf, DocumentEdits>,
    uri: &str,
    version: Option<i64>,
    edits: &Value,
) -> Result<(), String> {
    let url = reqwest::Url::parse(uri).map_err(|error| error.to_string())?;
    if url.scheme() != "file" || url.query().is_some() || url.fragment().is_some() {
        return Err("workspace edit requires a local file URI".into());
    }
    let path = url
        .to_file_path()
        .map_err(|_| "workspace edit requires a local file URI")?;
    let edits = edits
        .as_array()
        .ok_or("edits must be an array")?
        .iter()
        .map(parse_text_edit)
        .collect::<Result<Vec<_>, _>>()?;
    if out.insert(path, DocumentEdits { version, edits }).is_some() {
        return Err("repeated document edits are unsupported; no files changed".into());
    }
    Ok(())
}

fn parse_text_edit(edit: &Value) -> Result<TextEdit, String> {
    let position = |edge: &str, field: &str| {
        edit["range"][edge][field]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("invalid edit {} {}", edge, field))
    };
    Ok(TextEdit {
        start_line: position("start", "line")?,
        start_char: position("start", "character")?,
        end_line: position("end", "line")?,
        end_char: position("end", "character")?,
        new_text: edit["newText"]
            .as_str()
            .ok_or("newText must be a string")?
            .to_string(),
    })
}

pub(crate) fn apply_edits(
    content: &str,
    edits: &[TextEdit],
    encoding: PositionEncoding,
) -> Result<String, String> {
    let mut ranges = edits
        .iter()
        .map(|edit| {
            let start = encoding
                .offset(content, edit.start_line, edit.start_char)
                .ok_or("edit start position out of range or inside encoded character")?;
            let end = encoding
                .offset(content, edit.end_line, edit.end_char)
                .ok_or("edit end position out of range or inside encoded character")?;
            if start > end {
                return Err("edit range is reversed".to_string());
            }
            Ok((start, end, &edit.new_text))
        })
        .collect::<Result<Vec<_>, String>>()?;
    ranges.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 || pair[0].0 == pair[1].0 {
            return Err("overlapping or ambiguous edit ranges".into());
        }
    }
    let mut after = content.to_string();
    for (start, end, replacement) in ranges.into_iter().rev() {
        after.replace_range(start..end, replacement);
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file_uri(name: &str) -> String {
        reqwest::Url::from_file_path(std::env::current_dir().unwrap().join(name))
            .unwrap()
            .to_string()
    }

    fn edit(start: usize, end: usize, text: &str) -> Value {
        json!({"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"newText":text})
    }

    #[test]
    fn malformed_workspace_shapes_fail_instead_of_becoming_empty_edits() {
        let uri = file_uri("fixture.rs");
        for (value, reason) in [
            (json!({}), "neither changes nor documentChanges"),
            (json!({"changes":[]}), "changes must be an object"),
            (
                json!({"documentChanges":{}}),
                "documentChanges must be an array",
            ),
            (
                json!({"documentChanges":[{"edits":[]}]}),
                "textDocument.uri is required",
            ),
            (
                json!({"changes":{(uri.as_str()):{}}}),
                "edits must be an array",
            ),
        ] {
            assert!(
                collect_workspace_edit(&value)
                    .err()
                    .unwrap()
                    .contains(reason),
                "{value}"
            );
        }
        for value in [json!({"changes":{}}), json!({"documentChanges":[]})] {
            assert!(collect_workspace_edit(&value).unwrap().is_empty());
        }
    }

    #[test]
    fn only_local_file_uris_without_query_or_fragment_are_accepted() {
        let uri = file_uri("a.rs");
        for uri in [
            "https://example.test/a.rs".to_string(),
            "untitled:buffer".to_string(),
            format!("{uri}?version=1"),
            format!("{uri}#symbol"),
        ] {
            let value = json!({"changes":{(uri.as_str()):[edit(0, 0, "x")]}});
            assert!(
                collect_workspace_edit(&value)
                    .err()
                    .unwrap()
                    .contains("local file URI"),
                "{uri}"
            );
        }
    }

    #[test]
    fn versions_and_confirmation_requirements_are_validated() {
        let uri = file_uri("fixture.rs");
        for version in [json!("1"), json!(1.5), json!(true), json!(u64::MAX)] {
            let value = json!({"documentChanges":[{
                "textDocument":{"uri":uri,"version":version},"edits":[]
            }]});
            assert_eq!(
                collect_workspace_edit(&value).err().unwrap(),
                "invalid document version"
            );
        }
        for document in [json!({"uri":uri}), json!({"uri":uri,"version":null})] {
            let edits = collect_workspace_edit(
                &json!({"documentChanges":[{"textDocument":document,"edits":[]}]}),
            )
            .unwrap();
            assert_eq!(edits.values().next().unwrap().version, None);
        }
        for needs_confirmation in [false, true] {
            let value = json!({"changes":{(uri.as_str()):[edit(0, 0, "x")]},
                "changeAnnotations":{"a":{"label":"rename","needsConfirmation":needs_confirmation}}});
            let result = collect_workspace_edit(&value);
            if needs_confirmation {
                assert!(result.err().unwrap().contains("confirmation"));
            } else {
                assert_eq!(result.unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn each_position_component_requires_a_nonnegative_integer() {
        for edge in ["start", "end"] {
            for field in ["line", "character"] {
                for invalid in [Value::Null, json!(-1), json!(0.5), json!("0"), json!(true)] {
                    let mut value = edit(0, 0, "x");
                    value["range"][edge][field] = invalid;
                    assert_eq!(
                        parse_text_edit(&value).err().unwrap(),
                        format!("invalid edit {edge} {field}")
                    );
                }
            }
        }
        let mut value = edit(0, 0, "x");
        value["newText"] = Value::Null;
        assert_eq!(
            parse_text_edit(&value).err().unwrap(),
            "newText must be a string"
        );
    }

    #[test]
    fn unordered_adjacent_edits_use_original_offsets_in_every_encoding() {
        for (encoding, end) in [
            (PositionEncoding::Utf8, 5),
            (PositionEncoding::Utf16, 3),
            (PositionEncoding::Utf32, 2),
        ] {
            let edits = [edit(end, end + 1, "!"), edit(1, end, "éé"), edit(0, 1, "")]
                .iter()
                .map(parse_text_edit)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(apply_edits("a😀z", &edits, encoding).unwrap(), "éé!");
            assert_eq!(apply_edits("a😀z", &[], encoding).unwrap(), "a😀z");
        }
    }

    #[test]
    fn multiline_edits_consume_crlf_and_allow_insertion_at_trailing_empty_line() {
        let edits = [
            json!({"range":{"start":{"line":0,"character":1},"end":{"line":1,"character":1}},"newText":"X\nY"}),
            json!({"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":0}},"newText":"tail"}),
        ].iter().map(parse_text_edit).collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            apply_edits("a😀\r\nβz\n", &edits, PositionEncoding::Utf16).unwrap(),
            "aX\nYz\ntail"
        );
        let mut value = edit(0, 0, "x");
        value["range"]["end"]["line"] = json!(3);
        let invalid = parse_text_edit(&value).unwrap();
        assert!(apply_edits("a\n", &[invalid], PositionEncoding::Utf16)
            .unwrap_err()
            .contains("edit end position"));
    }

    #[test]
    fn resource_operations_and_malformed_edits_are_not_dropped() {
        let uri = file_uri("fixture.rs");
        for kind in ["create", "rename", "delete"] {
            let result =
                collect_workspace_edit(&json!({"documentChanges":[{"kind":kind,"uri":uri}]}));
            assert!(result.err().unwrap().contains("resource operation"));
        }
        for (malformed, reason) in [
            (json!({}), "invalid edit start line"),
            (
                json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}}}),
                "newText must be a string",
            ),
        ] {
            assert_eq!(
                collect_workspace_edit(&json!({"changes":{(uri.as_str()):[malformed]}}))
                    .err()
                    .unwrap(),
                reason
            );
        }
    }

    #[test]
    fn document_versions_and_percent_encoded_paths_are_preserved() {
        let uri = file_uri("a b.rs");
        assert!(uri.ends_with("/a%20b.rs"));
        let edits = collect_workspace_edit(&json!({"documentChanges":[{
            "textDocument":{"uri":uri,"version":17},"edits":[edit(0,0,"x")]
        }]}))
        .unwrap();
        let (path, document) = edits.first_key_value().unwrap();
        assert_eq!(path, &std::env::current_dir().unwrap().join("a b.rs"));
        assert_eq!(document.version, Some(17));
        assert_eq!(
            apply_edits("", &document.edits, PositionEncoding::Utf16).unwrap(),
            "x"
        );
    }

    #[test]
    fn edit_ranges_are_checked_before_splicing() {
        let edits = [edit(3, 4, "X"), edit(0, 1, "A")]
            .iter()
            .map(parse_text_edit)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            apply_edits("a😀z\r\n", &edits, PositionEncoding::Utf16).unwrap(),
            "A😀X\r\n"
        );
        for invalid in [
            vec![edit(2, 3, "x")],
            vec![edit(4, 5, "x")],
            vec![edit(3, 1, "x")],
            vec![edit(0, 3, "x"), edit(1, 4, "y")],
            vec![edit(1, 1, "x"), edit(1, 1, "y")],
        ] {
            let edits = invalid
                .iter()
                .map(parse_text_edit)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(apply_edits("a😀z\r\n", &edits, PositionEncoding::Utf16).is_err());
        }
    }

    #[test]
    fn repeated_document_batches_and_mixed_forms_are_rejected() {
        let uri = file_uri("fixture.rs");
        let document = json!({"textDocument":{"uri":uri,"version":1},"edits":[edit(0,0,"x")]});
        assert_eq!(
            collect_workspace_edit(&json!({"documentChanges":[document, document]}))
                .err()
                .unwrap(),
            "repeated document edits are unsupported; no files changed"
        );
        assert_eq!(
            collect_workspace_edit(&json!({"changes":{},"documentChanges":[]}))
                .err()
                .unwrap(),
            "workspace edit contains both changes and documentChanges"
        );
    }
}
