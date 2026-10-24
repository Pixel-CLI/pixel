// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Apply semantic JSON changes while retaining the spelling of untouched values.

use serde_json::Value;

/// Patch a validated JSON document; unknown or malformed input is never guessed at.
pub(crate) fn patch(text: &str, value: &Value) -> serde_json::Result<String> {
    let old: Value = serde_json::from_str(text)?;
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    let result = format!(
        "{}{}{}",
        &text[..start],
        edit(&text[start..end], &old, value)?,
        &text[end..]
    );
    if serde_json::from_str::<Value>(&result)? != *value {
        return Err(serde_json::Error::io(std::io::Error::other(
            "JSON edit changed unrelated values",
        )));
    }
    Ok(result)
}

fn token(text: &str, pos: &mut usize) -> serde_json::Result<(usize, usize, Value)> {
    *pos += text[*pos..].len() - text[*pos..].trim_start().len();
    let start = *pos;
    let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    let value = stream.next().expect("validated container has a value")?;
    *pos += stream.byte_offset();
    Ok((start, *pos, value))
}

fn edit(text: &str, old: &Value, new: &Value) -> serde_json::Result<String> {
    if old == new {
        return Ok(text.into());
    }
    if !((old.is_object() && new.is_object()) || (old.is_array() && new.is_array())) {
        return serde_json::to_string(new);
    }
    if let (Some(old), Some(new)) = (old.as_array(), new.as_array()) {
        return edit_array(text, old, new);
    }
    let count = old.as_object().expect("object pair").len();
    let mut pos = 1;
    let mut members = Vec::with_capacity(count);
    while {
        pos += text[pos..].len() - text[pos..].trim_start().len();
        text.as_bytes()[pos] != b'}'
    } {
        let (start, _, value) = token(text, &mut pos)?;
        let key = value.as_str().expect("validated object key").to_owned();
        pos += text[pos..].len() - text[pos..].trim_start().len();
        pos += 1; // colon in the validated object
        let (value_start, end, value) = token(text, &mut pos)?;
        let key = (key, value_start);
        members.push((start, end, key, value));
        pos += text[pos..].len() - text[pos..].trim_start().len();
        if text.as_bytes()[pos] == b',' {
            pos += 1;
        }
    }
    let mut rendered = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for (index, (start, end, (key, value_start), value)) in members.iter().enumerate() {
        if let Some(replacement) = new.get(key) {
            used.insert(key.clone());
            rendered.push((
                *start,
                *end,
                if old.get(key) == Some(replacement)
                    || members[index + 1..]
                        .iter()
                        .any(|(_, _, (later, _), _)| later == key)
                {
                    text[*start..*end].to_owned()
                } else {
                    format!(
                        "{}{}",
                        &text[*start..*value_start],
                        edit(&text[*value_start..*end], value, replacement)?
                    )
                },
            ));
        }
    }
    let mut out = text[..1].to_owned();
    if let Some((start, _, _, _)) = members.first() {
        out.push_str(&text[1..*start]);
    }
    let mut previous = None;
    for (start, end, member) in rendered {
        if let Some(previous_end) = previous {
            // Removed members can lie between retained ones. Keep just the separator.
            if members
                .iter()
                .any(|(s, _, _, _)| *s >= previous_end && *s < start)
            {
                out.push(',');
            } else {
                out.push_str(&text[previous_end..start]);
            }
        }
        out.push_str(&member);
        previous = Some(end);
    }
    let additions = new
        .as_object()
        .expect("object pair")
        .iter()
        .filter(|(key, _)| !used.contains(*key))
        .map(|(key, value)| {
            Ok(format!(
                "{}{}:{}{}",
                if members.is_empty() { "" } else { "\n  " },
                serde_json::to_string(key)?,
                if members.is_empty() { "" } else { " " },
                serde_json::to_string_pretty(value)?.replace('\n', "\n  ")
            ))
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    for addition in additions {
        if previous.is_some() {
            out.push(',');
        }
        out.push_str(&addition);
        previous = Some(0);
    }
    let tail = members.last().map_or(1, |(_, end, _, _)| *end);
    if members.is_empty() {
        out.push_str(&text[1..]);
    } else {
        out.push_str(&text[tail..]);
    }
    Ok(out)
}

fn edit_array(text: &str, old: &[Value], new: &[Value]) -> serde_json::Result<String> {
    let mut pos = 1;
    let mut spans = Vec::with_capacity(old.len());
    for _ in old {
        let (start, end, value) = token(text, &mut pos)?;
        spans.push((start, end, value));
        pos += text[pos..].len() - text[pos..].trim_start().len();
        if text.as_bytes()[pos] == b',' {
            pos += 1;
        }
    }
    let mut out = "[".to_owned();
    if let Some((start, _, _)) = spans.first() {
        out.push_str(&text[1..*start]);
    }
    let mut used = std::collections::BTreeSet::new();
    let mut previous = None;
    for value in new {
        let matching = spans
            .iter()
            .enumerate()
            .find(|(i, (_, _, candidate))| !used.contains(i) && candidate == value)
            .or_else(|| {
                spans
                    .iter()
                    .enumerate()
                    .filter(|(i, (_, _, candidate))| {
                        !used.contains(i)
                            && candidate.is_object()
                            && value.is_object()
                            && !new.contains(candidate)
                    })
                    .max_by_key(|(_, (_, _, candidate))| {
                        candidate
                            .as_object()
                            .expect("object candidate")
                            .iter()
                            .filter(|(key, old)| value.get(*key) == Some(*old))
                            .count()
                    })
            });
        if let Some((index, (start, end, original))) = matching {
            if let Some(prior) = previous {
                if index > 0 && prior == spans[index - 1].1 {
                    out.push_str(&text[prior..*start]);
                } else {
                    out.push(',');
                }
            }
            out.push_str(&edit(&text[*start..*end], original, value)?);
            used.insert(index);
            previous = Some(*end);
        } else {
            if previous.is_some() {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(value)?);
            previous = Some(0);
        }
    }
    let tail = spans.last().map_or(1, |(_, end, _)| *end);
    out.push_str(&text[tail..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patch_should_preserve_user_tokens_across_a_round_trip() {
        for text in [
            "{\"theme\" : \"dark\", \"number\":1e2}\n",
            "{\n\t\"packages\" : [ \"mine\", 1.00 ]\n}\n",
            "{}",
            "{  }",
        ] {
            let original: Value = serde_json::from_str(text).unwrap();
            let mut installed = original.clone();
            installed["hooks"] = json!({"SessionStart": [{"pixel": true}]});
            let changed = patch(text, &installed).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&changed).unwrap(), installed);
            assert_eq!(patch(&changed, &original).unwrap(), text);
        }
    }

    #[test]
    fn patch_should_leave_duplicate_user_keys_untouched_when_adding_hooks() {
        let text = r#"{ "mine": 1e2, "mine" : 2 }"#;
        let mut installed: Value = serde_json::from_str(text).unwrap();
        installed["hooks"] = json!({"pixel": true});
        let changed = patch(text, &installed).unwrap();
        assert!(changed.contains(r#""mine": 1e2, "mine" : 2"#));
        installed.as_object_mut().unwrap().remove("hooks");
        assert_eq!(patch(&changed, &installed).unwrap(), text);
    }

    #[test]
    fn patch_should_change_only_the_effective_duplicate_key() {
        let text = r#"{"hooks": { "inactive" : 1e2 }, "hooks": {}}"#;
        let installed = json!({"hooks": {"pixel": true}});
        let changed = patch(text, &installed).unwrap();
        assert!(changed.contains(r#""hooks": { "inactive" : 1e2 }"#));
        assert_eq!(serde_json::from_str::<Value>(&changed).unwrap(), installed);
    }

    #[test]
    fn patch_should_honor_array_order_and_duplicate_counts() {
        assert_eq!(patch("[1,2,1]", &json!([2, 1, 1, 3])).unwrap(), "[2,1,1,3]");
    }

    #[test]
    fn patch_should_delete_members_without_losing_the_remaining_values() {
        let text = r#"{ "a": 1, "b": 2, "c": 3 }"#;
        for value in [
            json!({"a":1,"c":3}),
            json!({"b":2}),
            json!({}),
            json!({"c":3,"d":4}),
        ] {
            let result = patch(text, &value).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&result).unwrap(), value);
        }
    }

    #[test]
    fn patch_should_keep_user_tokens_when_a_shared_hook_group_loses_a_pixel_entry() {
        let text = r#"{"hooks":[{ "matcher" : "*", "hooks" : [ { "command" : "mine", "timeout": 1e2 }, {"command":"pixel"} ]}]}"#;
        let mut value: Value = serde_json::from_str(text).unwrap();
        value["hooks"][0]["hooks"].as_array_mut().unwrap().pop();
        let result = patch(text, &value).unwrap();
        assert!(result.contains(r#"{ "command" : "mine", "timeout": 1e2 }"#));
        assert!(result.contains(r#""matcher" : "*", "hooks" : [ "#));
        assert!(!result.contains("pixel"));
    }

    #[test]
    fn patch_should_keep_array_entries_and_nested_user_formatting() {
        let text = "{\"hooks\" : {\"Start\" : [ {\"mine\": 1e2} ]}, \"other\":true}";
        let original: Value = serde_json::from_str(text).unwrap();
        let mut installed = original.clone();
        installed["hooks"]["Start"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pixel":true}));
        let changed = patch(text, &installed).unwrap();
        assert!(changed.contains("{\"mine\": 1e2}"));
        assert_eq!(patch(&changed, &original).unwrap(), text);
        assert!(patch("broken", &json!({})).is_err());
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_should_replace_incompatible_containers_instead_of_treating_them_as_pairs() {
        for (text, value, expected) in [
            ("{ \"a\" : 1e2 }", json!([1]), "[1]"),
            ("[ 1e2 ]", json!({"b":2}), "{\"b\":2}"),
            ("[ 1e2 ]", json!(true), "true"),
            ("1e2", json!([1]), "[1]"),
            ("true", json!({"a":1}), "{\"a\":1}"),
        ] {
            assert_eq!(patch(text, &value).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn array_edits_should_consume_duplicates_once_and_preserve_only_adjacent_separators() {
        for (text, value, expected) in [
            ("[ 1e2, 2, 3 ]", json!([3, 100.0]), "[ 3,1e2 ]"),
            ("[ 1, 1, 2 ]", json!([1, 2]), "[ 1,2 ]"),
            ("[ 1, 2, 3 ]", json!([2, 3]), "[ 2, 3 ]"),
            ("[ 1, 2 ]", json!([9, 1]), "[ 9,1 ]"),
            ("[ 1, 2 ]", json!([]), "[  ]"),
        ] {
            assert_eq!(patch(text, &value).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn array_edits_should_patch_the_most_similar_unused_object_without_reusing_retained_objects() {
        let text = r#"[ { "id":1, "v":1e2 }, { "id":2, "v":2e2 } ]"#;
        let changed = json!([{"id":2,"v":201},{"id":1,"v":100.0}]);
        assert_eq!(
            patch(text, &changed).unwrap(),
            r#"[ { "id":2, "v":201 },{ "id":1, "v":1e2 } ]"#
        );
    }
}
