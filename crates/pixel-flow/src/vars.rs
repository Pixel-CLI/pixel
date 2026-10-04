// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Flow-variable resolution shared by `replay` (text output), `execute`
//! (live browser runs) and `pixel ultraflow`'s condition evaluator, so a
//! declared default resolves the same way in all of them.

use std::collections::HashMap;

use crate::types::{FlowStep, FlowVar};

/// Resolve a step's value: `value_var` takes precedence, then `value`,
/// then the empty string.
///
/// A `value_var` the caller did not pass falls back to the variable's
/// declared `default`, then to a `{{var}}` placeholder for the agent to
/// replace.
pub(crate) fn resolve_value(
    step: &FlowStep,
    vars: &HashMap<String, String>,
    flow_vars: &[FlowVar],
) -> String {
    if let Some(ref var_name) = step.value_var {
        if let Some(v) = vars.get(var_name) {
            return v.clone();
        }
        if let Some(default) = declared_default(flow_vars, var_name) {
            return default.to_string();
        }
        return format!("{{{{{var_name}}}}}");
    }
    if let Some(ref v) = step.value {
        return substitute(v, vars);
    }
    String::new()
}

/// The `default` a flow declares for `name`, when it declares one.
fn declared_default<'a>(flow_vars: &'a [FlowVar], name: &str) -> Option<&'a str> {
    flow_vars
        .iter()
        .find(|v| v.name == name)
        .and_then(|v| v.default.as_deref())
}

/// Substitute `{{var}}` templates in a string.
///
/// Public so every reader of a step's text — the executor, the text emitter
/// and `pixel ultraflow`'s condition evaluator — substitutes the same way.
pub fn substitute(s: &str, vars: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (k, v) in vars {
        let placeholder = format!("{{{{{k}}}}}");
        result = result.replace(&placeholder, v);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: &str) -> FlowStep {
        FlowStep {
            action: action.into(),
            ..Default::default()
        }
    }

    fn var(name: &str, default: Option<&str>) -> FlowVar {
        FlowVar {
            name: name.into(),
            description: format!("{name} variable"),
            required: true,
            default: default.map(ToString::to_string),
        }
    }

    /// The caller's `--var` wins; a var absent from the map resolves to the
    /// default its flow declares — never to the literal placeholder, which
    /// would go into a real form.
    #[test]
    fn resolve_value_prefers_the_var_then_the_default_then_the_placeholder() {
        let vars = HashMap::from([("account".to_string(), "east".to_string())]);
        let declared = [var("account", Some("west"))];
        let account = FlowStep {
            value_var: Some("account".into()),
            ..step("fill")
        };
        assert_eq!(resolve_value(&account, &vars, &declared), "east");
        assert_eq!(resolve_value(&account, &HashMap::new(), &declared), "west");

        let code = FlowStep {
            value_var: Some("code".into()),
            ..step("fill")
        };
        assert_eq!(
            resolve_value(&code, &HashMap::new(), &[var("code", None)]),
            "{{code}}"
        );
    }

    #[test]
    fn resolve_value_falls_back_to_the_literal_value_then_to_nothing() {
        let vars = HashMap::from([("who".to_string(), "alice".to_string())]);
        let literal = FlowStep {
            value: Some("hi {{who}}".into()),
            ..step("fill")
        };
        assert_eq!(resolve_value(&literal, &vars, &[]), "hi alice");
        assert_eq!(resolve_value(&step("fill"), &vars, &[]), "");
    }

    #[test]
    fn substitute_replaces_every_placeholder_and_keeps_unknown_ones() {
        let vars = HashMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        assert_eq!(
            substitute("{{a}}+{{b}}={{a}}{{b}} {{c}}", &vars),
            "1+2=12 {{c}}"
        );
        assert_eq!(substitute("plain", &vars), "plain");
    }
}
