// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The observation: `agent-browser snapshot -i` parsed into numbered slots.
//!
//! One node receives one slot, even when it supports both clicking and
//! typing — the same rule the indexed action space is built on. Only
//! elements a supported operation can act on get a slot; the rest of the
//! accessibility tree is shown as page context so a decision can reason
//! about where it is, but it can never be named as a target.

use serde::{Deserialize, Serialize};

/// Roles that offer `CLICK`. Toggling a checkbox/radio/switch is a click.
const CLICK_ROLES: &[&str] = &[
    "button",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "checkbox",
    "radio",
    "switch",
    "summary",
    "slider",
    "option",
];
/// Roles that offer `TYPE` (fill).
const TYPE_ROLES: &[&str] = &["textbox", "searchbox", "spinbutton", "textarea"];
/// Roles whose nature the accessibility tree does not settle: agent-browser
/// reports both a native `<select>` and an ARIA combobox as `combobox`, so
/// both operations are offered and the decision picks.
const AMBIGUOUS_ROLES: &[&str] = &["combobox"];

/// Most elements parsed from one snapshot. A runaway page must bound the
/// work and the prompt, not the process; the dropped tail is disclosed.
pub const MAX_ELEMENTS: usize = 400;

/// One control observed on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// The 1-based slot a decision names, assigned in snapshot order to
    /// elements an operation can act on. `None` for page context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<usize>,
    /// agent-browser's ephemeral handle for the node (`e185`).
    pub reference: String,
    /// The role agent-browser printed (`button`, `textbox`, `combobox`, ...).
    pub role: String,
    /// The accessible name.
    pub name: String,
    /// Current value, when the element carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// `checked` state, when the element reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// A control the tree marks unavailable: shown, never offered.
    pub disabled: bool,
    /// agent-browser marked the node `clickable` — the only signal that
    /// covers roles it synthesizes (`LabelText`).
    pub clickable: bool,
}

impl Element {
    /// Whether an operation can act on this element at all.
    pub fn is_actionable(&self) -> bool {
        !self.disabled
            && (self.clickable
                || CLICK_ROLES.contains(&self.role.as_str())
                || TYPE_ROLES.contains(&self.role.as_str())
                || AMBIGUOUS_ROLES.contains(&self.role.as_str()))
    }

    /// The operations this element offers, in a fixed order so two runs
    /// over one page build the same action space.
    pub fn operations(&self) -> Vec<super::action::Op> {
        use super::action::Op;
        let mut ops = Vec::new();
        if self.clickable || CLICK_ROLES.contains(&self.role.as_str()) {
            ops.push(Op::Click);
        }
        if TYPE_ROLES.contains(&self.role.as_str()) {
            ops.push(Op::Type);
        }
        if AMBIGUOUS_ROLES.contains(&self.role.as_str()) {
            ops.push(Op::Type);
            ops.push(Op::Select);
        }
        ops
    }

    /// The criterion shown beside an offered target: what the element is,
    /// what it already holds, and what state it is in.
    pub fn describe(&self) -> String {
        let mut out = format!("{} \"{}\"", self.role, self.name);
        if let Some(value) = self.value.as_deref().filter(|v| !v.is_empty()) {
            out.push_str(&format!(", holding \"{value}\""));
        }
        match self.checked {
            Some(true) => out.push_str(", currently checked"),
            Some(false) => out.push_str(", currently unchecked"),
            None => {}
        }
        out
    }

    /// The hint a flow step records so replay can re-resolve this element
    /// against a fresh snapshot. Refs are ephemeral, so a step never stores
    /// one: it stores the role and the accessible name, which
    /// `pixel_flow::execute` already knows how to match.
    ///
    /// Apostrophes are dropped — the hint is read by a single-quote-driven
    /// matcher, and `What's New` must not read as two hints.
    pub fn ref_hint(&self) -> String {
        format!("{} containing '{}'", self.role, self.name.replace('\'', ""))
    }
}

/// One look at the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub url: String,
    /// The raw snapshot, kept for the condition fallback and the trace.
    pub snapshot: String,
    pub elements: Vec<Element>,
    /// Whether [`MAX_ELEMENTS`] cut the snapshot.
    pub truncated: bool,
}

impl Observation {
    /// Parse one look: the page URL and the interactive snapshot.
    pub fn of(url: String, snapshot: String) -> Observation {
        let (elements, truncated) = parse_snapshot(&snapshot);
        Observation {
            url,
            snapshot,
            elements,
            truncated,
        }
    }

    /// Look at the page through a browser: the URL, then its interactive
    /// snapshot. The one shape every observation takes, so discovery and
    /// replay see the page the same way.
    pub fn see(browser: &mut dyn pixel_flow::Browser) -> Result<Observation, String> {
        let url = browser.run(&["get", "url"])?;
        let snapshot = browser.run(&["snapshot", "-i"])?;
        Ok(Observation::of(url.trim().to_string(), snapshot))
    }

    /// The element the slot names.
    pub fn element(&self, slot: usize) -> Option<&Element> {
        self.elements.iter().find(|e| e.slot == Some(slot))
    }

    /// The page as a decision sees it: one line per element, its slot in
    /// front when it is a legal target. The whole point of the indexed
    /// action space is that this text and the offered labels agree, so a
    /// bracketed number is always choosable.
    pub fn table(&self) -> String {
        let mut out = String::new();
        for element in &self.elements {
            match element.slot {
                Some(slot) => out.push_str(&format!("[{slot}] {}", render(element))),
                None => out.push_str(&format!("[--] {}", render(element))),
            }
            out.push('\n');
        }
        if self.truncated {
            out.push_str(&format!(
                "[--] (snapshot cut at {MAX_ELEMENTS} elements; later controls are not listed)\n"
            ));
        }
        out
    }

    /// Whether the page moved since `before` — the staleness rule a step
    /// budget needs to notice it is looping.
    pub fn changed_since(&self, before: &Observation) -> bool {
        self.url != before.url || self.table() != before.table()
    }
}

/// Render one element as one decision-facing line.
fn render(element: &Element) -> String {
    let mut out = format!("{} \"{}\"", element.role, element.name);
    if let Some(value) = element.value.as_deref().filter(|v| !v.is_empty()) {
        out.push_str(&format!(" = \"{value}\""));
    }
    match element.checked {
        Some(true) => out.push_str(" (checked)"),
        Some(false) => out.push_str(" (unchecked)"),
        None => {}
    }
    if element.disabled {
        out.push_str(" (disabled)");
    }
    out
}

/// Parse an interactive snapshot into elements, assigning slots in
/// snapshot order. Returns whether the [`MAX_ELEMENTS`] cap cut the input.
pub fn parse_snapshot(snapshot: &str) -> (Vec<Element>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    for line in snapshot.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- ") else {
            continue;
        };
        if out.len() >= MAX_ELEMENTS {
            truncated = true;
            break;
        }
        let Some(element) = parse_line(rest) else {
            continue;
        };
        out.push(element);
    }
    let mut slot = 0usize;
    for element in &mut out {
        if element.is_actionable() {
            slot += 1;
            element.slot = Some(slot);
        }
    }
    (out, truncated)
}

/// Parse one snapshot line's body (already stripped of its `- ` marker).
fn parse_line(body: &str) -> Option<Element> {
    let (role, rest) = match body.split_once(' ') {
        Some((role, rest)) => (role, rest),
        None => (body, ""),
    };
    // A node can carry more than one bracketed block: agent-browser puts the
    // attributes in one and its own hints in another
    // (`LabelText "Menu" [ref=e82] clickable [cursor:pointer]`).
    let attrs = bracketed_spans(rest).join(",");
    let name = first_quoted(rest).unwrap_or_default();
    let reference = attr(&attrs, "ref")?;
    let checked = attr(&attrs, "checked").map(|value| value != "false");
    // agent-browser echoes a field's current value after the attribute
    // block (`- textbox "Search" [ref=e59]: filled text`). The element
    // table prints it; parse it too, so a decision can see the field is
    // already satisfied (issue #638).
    let echoed = rest
        .split('[')
        .skip(1)
        .find_map(|after_open| {
            let (span, after) = after_open.split_once(']')?;
            (attr(span, "ref").as_deref() == Some(reference.as_str())).then_some(after)
        })
        .and_then(|after| after.strip_prefix(": "))
        .map(str::trim)
        .filter(|held| !held.is_empty())
        .map(str::to_string);
    let value = attr(&attrs, "value").or(echoed);
    Some(Element {
        slot: None,
        reference,
        role: role.to_string(),
        name,
        value,
        checked,
        disabled: has_bare(&attrs, "disabled"),
        clickable: rest.contains("clickable"),
    })
}

/// Every bracketed span of a snapshot line, in order.
fn bracketed_spans(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((_, after_open)) = rest.split_once('[') {
        match after_open.split_once(']') {
            Some((span, tail)) => {
                out.push(span);
                rest = tail;
            }
            // An unclosed `[` opens nothing.
            None => break,
        }
    }
    out
}

/// The first double-quoted string in `text`.
fn first_quoted(text: &str) -> Option<String> {
    let open = text.find('"')?;
    let rest = &text[open + 1..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// The value of `key=` in a comma-separated attribute list.
///
/// A quoted value is read up to its closing quote, so a value that contains
/// a comma (`value="Zurich, CH"`) survives; a bare one ends at the next
/// comma. The scan walks on past attributes that are not `key`, which is
/// what lets it skip a comma inside a quoted value.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs.trim_start_matches([',', ' ']);
    loop {
        if let Some(after) = rest
            .strip_prefix(key)
            .and_then(|tail| tail.strip_prefix('='))
        {
            let after = after.trim_start();
            return Some(match after.strip_prefix('"') {
                Some(quoted) => quoted[..quoted.find('"')?].to_string(),
                None => after
                    .split([',', ' '])
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('"')
                    .to_string(),
            });
        }
        // Advance past this attribute — and past a comma inside a quoted
        // value, which is why this is not `split(',')`. `split_once` always
        // moves forward, so a malformed list cannot spin the scan.
        rest = rest.split_once(',')?.1.trim_start_matches(' ');
    }
}

/// Whether a comma-separated attribute list carries `key` with no value.
fn has_bare(attrs: &str, key: &str) -> bool {
    attrs.split(',').any(|part| part.trim() == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Op;

    /// A trimmed version of a real `agent-browser snapshot -i` body:
    /// nested containers, a bare `clickable` hint, and `key=value` attrs.
    const DUCK: &[&str] = &[
        "- link \"Learn about DuckDuckGo\" [ref=e80]",
        "- radio \"Search\" [checked=true, ref=e158]",
        "- radio \"Ask AI\" [checked=false, ref=e159]",
        "- combobox \"Search with DuckDuckGo\" [expanded=false, required, ref=e185]",
        "- button \"Search\" [ref=e186]",
        "- LabelText \"Menu\" [ref=e82] clickable [cursor:pointer]",
        "  - heading \"SEARCH\" [level=2, ref=e115]",
        "  - button \"Dismiss promotion\" [disabled, ref=e138]",
    ];

    fn page() -> Observation {
        Observation::of("https://duckduckgo.com/".to_string(), DUCK.join("\n"))
    }

    #[test]
    fn slots_are_assigned_to_actionable_elements_in_snapshot_order() {
        let obs = page();
        let slots: Vec<(&str, Option<usize>)> = obs
            .elements
            .iter()
            .map(|e| (e.name.as_str(), e.slot))
            .collect();
        assert_eq!(
            slots,
            vec![
                ("Learn about DuckDuckGo", Some(1)),
                ("Search", Some(2)),
                ("Ask AI", Some(3)),
                ("Search with DuckDuckGo", Some(4)),
                ("Search", Some(5)),
                ("Menu", Some(6)),
                // A heading is page context: never a target.
                ("SEARCH", None),
                // A disabled control is shown and never offered, even though
                // its role would otherwise be clickable.
                ("Dismiss promotion", None),
            ]
        );
        // The slot resolves back to the element a decision named.
        assert_eq!(obs.element(4).unwrap().role, "combobox");
        assert_eq!(obs.element(4).unwrap().reference, "e185");
        assert_eq!(obs.element(99), None);
    }

    #[test]
    fn attributes_and_nesting_are_read_from_the_real_shapes() {
        let obs = page();
        let radio = &obs.elements[1];
        assert_eq!(radio.role, "radio");
        assert_eq!(radio.checked, Some(true));
        assert_eq!(radio.value, None);
        assert!(!radio.disabled);
        assert!(!radio.clickable, "a plain node is not marked clickable");
        // A bare attribute is not a value.
        assert_eq!(obs.elements[2].checked, Some(false));
        // The synthesized role is actionable only because of `clickable`.
        let menu = &obs.elements[5];
        assert_eq!(menu.role, "LabelText");
        assert!(menu.clickable);
        assert!(menu.is_actionable());
        assert!(obs.elements[7].disabled);
        assert!(!obs.elements[7].is_actionable());
        // Unknown roles are context.
        assert!(!obs.elements[6].is_actionable());
        assert!(obs.elements[6].operations().is_empty());
    }

    #[test]
    fn operations_follow_the_role_and_the_ambiguous_combobox_offers_both() {
        let obs = page();
        assert_eq!(obs.elements[0].operations(), vec![Op::Click]);
        assert_eq!(obs.elements[3].operations(), vec![Op::Type, Op::Select]);
        assert_eq!(obs.elements[5].operations(), vec![Op::Click]);
    }

    #[test]
    fn echoed_value_is_read_when_name_contains_reference_text() {
        let (elements, _) = parse_snapshot("- textbox \"Search ref=e59]\" [ref=e59]: filled text");
        assert_eq!(elements[0].value.as_deref(), Some("filled text"));
    }

    #[test]
    fn a_value_is_read_quoted_or_bare_and_empty_is_preserved() {
        let (elements, _) = parse_snapshot(
            "- textbox \"Where from?\" [value=\"San Francisco\", ref=e7]\n\
             - textbox \"Where to?\" [value=\"\", ref=e8]\n\
             - textbox \"Bare\" [value=Zurich, ref=e9]",
        );
        assert_eq!(elements[0].value.as_deref(), Some("San Francisco"));
        assert_eq!(elements[1].value.as_deref(), Some(""));
        assert_eq!(elements[2].value.as_deref(), Some("Zurich"));
        assert_eq!(elements[0].slot, Some(1));
        // An empty value is not rendered: it says nothing a decision needs,
        // while the criterion still names the field.
        assert!(!elements[1].describe().contains("holding"));
        // An empty value is not rendered into the table either: ` = ""` says
        // nothing a decision needs and would read as a typed value.
        let observation = Observation::of(
            String::new(),
            "- textbox \"Where to?\" [value=\"\", ref=e8]".to_string(),
        );
        assert!(observation.table().contains("[1] textbox \"Where to?\""));
        assert!(!observation.table().contains(" = "));
        assert!(elements[0].describe().contains("holding \"San Francisco\""));
    }

    #[test]
    fn the_table_numbers_only_what_a_decision_may_name() {
        let table = page().table();
        assert_eq!(
            table,
            "[1] link \"Learn about DuckDuckGo\"\n\
             [2] radio \"Search\" (checked)\n\
             [3] radio \"Ask AI\" (unchecked)\n\
             [4] combobox \"Search with DuckDuckGo\"\n\
             [5] button \"Search\"\n\
             [6] LabelText \"Menu\"\n\
             [--] heading \"SEARCH\"\n\
             [--] button \"Dismiss promotion\" (disabled)\n",
            "every bracketed number must be a legal target"
        );
    }

    #[test]
    fn a_line_without_a_ref_is_not_an_element() {
        let (elements, truncated) =
            parse_snapshot("- heading \"No handle here\"\n- not a bullet\nplain text\n");
        assert!(elements.is_empty(), "{elements:?}");
        assert!(!truncated);
    }

    /// The cap bounds the prompt: elements past it are dropped and the
    /// observation says so, so a truncated page is never passed off as
    /// complete.
    #[test]
    fn the_element_cap_truncates_and_discloses() {
        let body: Vec<String> = (1..=MAX_ELEMENTS + 5)
            .map(|i| format!("- button \"b{i}\" [ref=e{i}]"))
            .collect();
        let (elements, truncated) = parse_snapshot(&body.join("\n"));
        assert_eq!(elements.len(), MAX_ELEMENTS);
        assert!(truncated);
        assert!(!page().truncated);
        let obs = Observation::of(String::new(), body.join("\n"));
        assert!(obs.table().contains("snapshot cut at 400 elements"));
        // A snapshot inside the cap is never reported as cut.
        let (_, truncated) = parse_snapshot(&body[..MAX_ELEMENTS].join("\n"));
        assert!(!truncated);
    }

    #[test]
    fn staleness_compares_the_url_and_the_numbered_table() {
        let before = page();
        assert!(!before.changed_since(&page()));
        // Same elements, different URL: the page moved.
        let moved = Observation::of("https://duckduckgo.com/?q=x".to_string(), DUCK.join("\n"));
        assert!(before.changed_since(&moved));
        // Same URL, one more control: the page moved.
        let grown = Observation::of(
            before.url.clone(),
            format!("{}\n- button \"More\" [ref=e200]", DUCK.join("\n")),
        );
        assert!(before.changed_since(&grown));
        assert!(grown.changed_since(&before));
    }

    /// One look is one `get url` and one interactive snapshot, in that
    /// order, and the URL is trimmed of its trailing newline.
    #[test]
    fn one_look_reads_the_url_then_the_snapshot() {
        let mut browser = crate::testutil::ScriptedBrowser::default();
        browser.ok("https://duckduckgo.com/\n");
        browser.ok(DUCK.join("\n").as_str());
        let observation = Observation::see(&mut browser).unwrap();
        assert_eq!(observation.url, "https://duckduckgo.com/");
        assert_eq!(
            browser.calls(),
            vec![vec!["get", "url"], vec!["snapshot", "-i"],]
        );
        assert_eq!(observation.elements.len(), DUCK.len());
        // A failing browser call is that call's error, not an empty page.
        let mut broken = crate::testutil::ScriptedBrowser::default();
        broken.fail("no browser");
        assert_eq!(Observation::see(&mut broken).unwrap_err(), "no browser");
    }
}
