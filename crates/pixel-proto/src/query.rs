// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use serde::{Deserialize, Serialize};

/// Explicit, source-aware retrieval intents understood by `pixel query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryKind {
    Auto,
    Locate,
    Scope,
    Impact,
    HistoryRecovery,
    Status,
}

/// Whether the query compiler proved one recipe or can only rank candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryStatus {
    Resolved,
    Ranked,
    Partial,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPlan {
    pub recipe: String,
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub intent: String,
    pub status: QueryStatus,
    pub plan: Vec<QueryPlan>,
    #[serde(default)]
    pub evidence: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

/// What the locate recipe runs, in order: resolve the phrase, show the
/// context of the symbols it singles out, list their callers' test files,
/// and rank files for the task when nothing resolved.
pub const LOCATE_OPERATIONS: [&str; 4] = ["resolve", "context", "callers", "targets_on_unresolved"];

/// Compiles only unmistakable V1 phrasings. Ambiguous prose is deliberately
/// returned as ranked plans rather than triggering broad retrieval.
pub fn compile_query(intent: &str, explicit: QueryKind) -> QueryResult {
    let normalized = intent.trim();
    let inferred = match explicit {
        QueryKind::Auto if normalized.starts_with("where is `") && normalized.ends_with('`') => {
            Some(QueryKind::Locate)
        }
        QueryKind::Auto if normalized.starts_with("what files implement ") => {
            Some(QueryKind::Scope)
        }
        QueryKind::Auto if normalized.starts_with("show impact of ") => Some(QueryKind::Impact),
        QueryKind::Auto
            if normalized.starts_with("restore ") || normalized.contains("working before") =>
        {
            Some(QueryKind::HistoryRecovery)
        }
        QueryKind::Auto if normalized == "status" || normalized == "what changed" => {
            Some(QueryKind::Status)
        }
        QueryKind::Auto => None,
        kind => Some(kind),
    };
    let plan = match inferred {
        Some(QueryKind::Locate) => vec![QueryPlan {
            recipe: "locate.v2".into(),
            operations: LOCATE_OPERATIONS.iter().map(|op| (*op).into()).collect(),
        }],
        Some(QueryKind::Scope) => vec![QueryPlan {
            recipe: "scope.v1".into(),
            operations: vec!["targets".into()],
        }],
        Some(QueryKind::Impact) => vec![QueryPlan {
            recipe: "impact.v1".into(),
            operations: vec!["impact".into()],
        }],
        Some(QueryKind::HistoryRecovery) => vec![QueryPlan {
            recipe: "history_recovery.v1".into(),
            operations: vec!["excavate".into()],
        }],
        Some(QueryKind::Status) => vec![QueryPlan {
            recipe: "status.v1".into(),
            operations: vec!["inspect".into(), "review".into(), "changes".into()],
        }],
        Some(QueryKind::Auto) | None => vec![
            QueryPlan {
                recipe: "locate.v2".into(),
                operations: LOCATE_OPERATIONS.iter().map(|op| (*op).into()).collect(),
            },
            QueryPlan {
                recipe: "scope.v1".into(),
                operations: vec!["targets".into()],
            },
        ],
    };
    QueryResult {
        intent: normalized.into(),
        status: if inferred.is_some() {
            QueryStatus::Resolved
        } else {
            QueryStatus::Ranked
        },
        plan,
        evidence: vec![],
        bundle: None,
    }
}

// ---------------------------------------------------------------------------
// locate.v2 — one bounded answer composed from resolve, context and callers
// ---------------------------------------------------------------------------

/// How far a composed locate answer gets. It speaks of locating code only:
/// not of a bug explained, a change made correct, or tests passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocateStatus {
    /// One symbol singled out by an exact tier, its source shown fresh.
    Located,
    /// Candidates in distinct places that the scores do not separate.
    Ambiguous,
    /// A candidate exists, but a weak tier found it or its source could not
    /// be shown fresh: read it before relying on it.
    NeedsReading,
    /// Nothing matched: the next step is a search beyond the concept index.
    NeedsSearch,
}

/// One resolve match as the locate recipe weighs it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocateCandidate {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol the match names (a symbol-tier match) or sits in (its
    /// owner); `None` for text outside every symbol.
    pub symbol: Option<String>,
    /// The symbol kind of a symbol-tier match (`function`, `method`, …).
    pub kind: Option<String>,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// Scores at most this far below the best do not separate two candidates.
/// A binary fraction (1/16), so a score exactly on the margin compares
/// exactly.
const TIE_MARGIN: f64 = 0.0625;

/// The candidates of a resolve response, best first, in its own order.
pub fn locate_candidates(resolve: &serde_json::Value) -> Vec<LocateCandidate> {
    let line = |m: &serde_json::Value, key: &str| {
        m[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    resolve["matches"]
        .as_array()
        .map(|matches| {
            matches
                .iter()
                .map(|m| LocateCandidate {
                    path: m["path"].as_str().unwrap_or_default().to_owned(),
                    start_line: line(m, "start_line"),
                    end_line: line(m, "end_line"),
                    symbol: if m["symbol_kind"].is_string() {
                        m["raw"].as_str().map(str::to_owned)
                    } else {
                        m["owner"].as_str().map(str::to_owned)
                    },
                    kind: m["symbol_kind"].as_str().map(str::to_owned),
                    score: m["score"].as_f64().unwrap_or(0.0),
                    reasons: m["reasons"]
                        .as_array()
                        .map(|r| {
                            r.iter()
                                .filter_map(|x| x.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The reason `resolve` gives a match that contains the whole phrase
/// (`concept_resolve::match_reasons` in pixel-graph).
const SUBSTRING_REASON: &str = "substring match";

impl LocateCandidate {
    /// The uid a symbol-tier match has when its name is its qualified name
    /// (`path#name#kind`, a free function or a type); a method's qualified
    /// name differs, and the caller falls back to the name.
    pub fn likely_uid(&self) -> Option<String> {
        Some(format!(
            "{}#{}#{}",
            self.path,
            self.symbol.as_ref()?,
            self.kind.as_ref()?
        ))
    }
}

/// The status of a locate answer from the resolve tier, the candidates
/// (best first), and whether the best one's source was shown fresh. Only an
/// exact tier (`ident`, `t0`) can locate or be ambiguous; a kind-directed
/// match (`t1`) or a word match holding the whole phrase (`t2` with a
/// substring reason) is a lead to read; a partial word overlap, a trigram or
/// a symbol-word fallback calls for a search.
pub fn locate_status(
    tier: Option<&str>,
    candidates: &[LocateCandidate],
    best_fresh: bool,
) -> LocateStatus {
    let Some(best) = candidates.first() else {
        return LocateStatus::NeedsSearch;
    };
    match tier {
        Some("ident" | "t0") => {
            let tied = candidates[1..].iter().any(|c| {
                best.score - c.score <= TIE_MARGIN
                    && (c.path != best.path || c.symbol != best.symbol)
            });
            if tied {
                LocateStatus::Ambiguous
            } else if best_fresh && best.symbol.is_some() {
                LocateStatus::Located
            } else {
                LocateStatus::NeedsReading
            }
        }
        Some("t1") => LocateStatus::NeedsReading,
        Some("t2") if best.reasons.iter().any(|r| r == SUBSTRING_REASON) => {
            LocateStatus::NeedsReading
        }
        _ => LocateStatus::NeedsSearch,
    }
}

/// The distinct symbols to show, best first, at most `max`: one per
/// (path, symbol), candidates outside every symbol skipped.
pub fn locate_targets(candidates: &[LocateCandidate], max: usize) -> Vec<&LocateCandidate> {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let mut out = Vec::new();
    for c in candidates {
        let Some(symbol) = c.symbol.as_deref() else {
            continue;
        };
        if seen.contains(&(c.path.as_str(), symbol)) {
            continue;
        }
        if out.len() == max {
            break;
        }
        seen.push((c.path.as_str(), symbol));
        out.push(c);
    }
    out
}

/// Percent of the budget the best target's context gets, and each other one.
const BEST_TARGET_SHARE: usize = 60;
const OTHER_TARGET_SHARE: usize = 15;

/// The context budget of each of `targets` targets out of `budget` tokens,
/// best first. The shares leave at least a tenth for the header, callers
/// and tests: 60 for the best, 15 for each of at most two others.
pub fn split_context_budget(budget: usize, targets: usize) -> Vec<usize> {
    (0..targets)
        .map(|index| {
            let share = if index == 0 {
                BEST_TARGET_SHARE
            } else {
                OTHER_TARGET_SHARE
            };
            budget * share / 100
        })
        .collect()
}

/// The uid of the candidate in an ambiguous-name response that lives in
/// `path`, so the context of the intended homonym can be asked by uid.
pub fn candidate_uid_in(response: &serde_json::Value, path: &str) -> Option<String> {
    response["candidates"]
        .as_array()?
        .iter()
        .find(|c| c["path"].as_str() == Some(path))
        .and_then(|c| c["uid"].as_str())
        .map(str::to_owned)
}

/// True for a path whose file is a test by the usual conventions: a
/// `tests`/`test`/`__tests__`/`spec` directory, or a `_test`, `.test`,
/// `.spec` or `test_` file name.
pub fn looks_like_test_path(path: &str) -> bool {
    let mut parts = path.split('/').rev();
    let file = parts.next().unwrap_or_default();
    let in_test_dir = parts.any(|dir| matches!(dir, "tests" | "test" | "__tests__" | "spec"));
    in_test_dir
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
}

/// Distinct test files among the callers a `uses` response lists, sorted.
pub fn caller_test_files(uses: &serde_json::Value) -> Vec<String> {
    let mut files: Vec<String> = uses["edges"]
        .as_array()
        .map(|edges| {
            edges
                .iter()
                .filter_map(|e| e["symbol"]["path"].as_str())
                .filter(|p| looks_like_test_path(p))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.dedup();
    files
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The concrete next step after a locate answer, `None` once located: pick
/// a homonym by uid, read the candidate, or search beyond the concept index
/// (an exact identifier search for a one-word phrase, task scoping
/// otherwise).
pub fn locate_next_action(
    status: LocateStatus,
    phrase: &str,
    intent: &str,
    target_uids: &[String],
    best: Option<&LocateCandidate>,
) -> Option<String> {
    match status {
        LocateStatus::Located => None,
        LocateStatus::Ambiguous => Some(match target_uids.get(1) {
            Some(uid) => format!(
                "pick the intended candidate by uid, e.g. pixel pack-context {}",
                shell_quote(uid)
            ),
            None => format!("pixel find-code {}", shell_quote(phrase)),
        }),
        LocateStatus::NeedsReading => best.map(|c| {
            format!(
                "read {}:{}-{} before relying on it",
                c.path, c.start_line, c.end_line
            )
        }),
        LocateStatus::NeedsSearch if !phrase.contains(char::is_whitespace) => {
            Some(format!("pixel search-content -F {}", shell_quote(phrase)))
        }
        LocateStatus::NeedsSearch => Some(format!(
            "pixel scope-task {} --no-manifest",
            shell_quote(intent)
        )),
    }
}

/// The words a context response's `caps` carry when its source differs from
/// the graph snapshot (`op_context` in pixel-daemon): stale, so not shown.
const STALE_CONTEXT_CAP: &str = "source differs from graph snapshot";

/// True when a context response withheld its source as stale. An empty
/// `text` alone is not that: a small budget leaves it empty too.
pub fn context_is_stale(context: &serde_json::Value) -> bool {
    context["caps"].as_array().is_some_and(|caps| {
        caps.iter()
            .any(|cap| cap.as_str().is_some_and(|c| c.contains(STALE_CONTEXT_CAP)))
    })
}

/// True when every response carrying a `snapshot` carries the same one.
pub fn same_snapshot(responses: &[&serde_json::Value]) -> bool {
    let mut snapshots = responses
        .iter()
        .map(|r| &r["snapshot"])
        .filter(|s| !s.is_null());
    let Some(first) = snapshots.next() else {
        return true;
    };
    snapshots.all(|s| s == first)
}

/// Fits a locate answer to `budget` as `tokens` measures it: while the
/// answer is over budget, empties the context text of its targets, the
/// least-ranked first, then states in `limits` how many texts it emptied
/// and by how much the answer still exceeds the budget. A target whose text
/// was already empty is not counted: emptying it saves nothing, and
/// counting it announced dropped context on an answer that showed none.
pub fn fit_locate_to_budget(
    answer: &mut serde_json::Value,
    budget: usize,
    tokens: impl Fn(&serde_json::Value) -> usize,
) {
    let count = answer["targets"].as_array().map_or(0, Vec::len);
    let mut dropped = 0usize;
    for index in (0..count).rev() {
        if tokens(answer) <= budget {
            break;
        }
        let text = &mut answer["targets"][index]["text"];
        if text.as_str().is_some_and(|t| !t.is_empty()) {
            *text = serde_json::Value::String(String::new());
            dropped += 1;
        }
    }
    let over = tokens(answer).saturating_sub(budget);
    let Some(limits) = answer["limits"].as_array_mut() else {
        return;
    };
    if dropped > 0 {
        limits.push(serde_json::Value::String(format!(
            "context text of {dropped} target(s) dropped to fit the budget"
        )));
    }
    if over > 0 {
        limits.push(serde_json::Value::String(format!(
            "the answer exceeds the budget by about {over} tokens without its dropped text"
        )));
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// One token per character of context text plus `overhead`: a measure
    /// a test can reason about, unlike the CLI's byte estimate.
    fn text_tokens(overhead: usize) -> impl Fn(&serde_json::Value) -> usize {
        move |answer| {
            overhead
                + answer["targets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["text"].as_str().unwrap().len())
                    .sum::<usize>()
        }
    }

    fn locate_answer(texts: &[&str]) -> serde_json::Value {
        let targets: Vec<serde_json::Value> = texts
            .iter()
            .map(|text| serde_json::json!({ "text": text }))
            .collect();
        serde_json::json!({ "targets": targets, "limits": ["earlier limit"] })
    }

    fn texts_of(answer: &serde_json::Value) -> Vec<&str> {
        answer["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["text"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn fit_locate_to_budget_should_drop_the_least_ranked_text_until_it_fits() {
        // 12 tokens for a budget of 8: dropping the last text lands exactly
        // on the budget, which fits, so the best two texts stay.
        let mut answer = locate_answer(&["aaaa", "bbbb", "cccc"]);
        fit_locate_to_budget(&mut answer, 8, text_tokens(0));
        assert_eq!(texts_of(&answer), ["aaaa", "bbbb", ""]);
        assert_eq!(
            answer["limits"],
            serde_json::json!([
                "earlier limit",
                "context text of 1 target(s) dropped to fit the budget"
            ])
        );

        // Within budget: nothing changes and nothing is announced.
        let mut answer = locate_answer(&["aaaa", "bbbb"]);
        fit_locate_to_budget(&mut answer, 8, text_tokens(0));
        assert_eq!(texts_of(&answer), ["aaaa", "bbbb"]);
        assert_eq!(answer["limits"], serde_json::json!(["earlier limit"]));
    }

    #[test]
    fn fit_locate_to_budget_should_not_count_a_text_that_was_already_empty() {
        // The two empty texts after the best one save nothing: only the best
        // text is dropped, and the answer, still over, says by how much.
        let mut answer = locate_answer(&["aaaa", "", ""]);
        fit_locate_to_budget(&mut answer, 5, text_tokens(10));
        assert_eq!(texts_of(&answer), ["", "", ""]);
        assert_eq!(
            answer["limits"],
            serde_json::json!([
                "earlier limit",
                "context text of 1 target(s) dropped to fit the budget",
                "the answer exceeds the budget by about 5 tokens without its dropped text"
            ])
        );

        // No text to drop at all: an answer over budget only says so.
        let mut answer = locate_answer(&["", ""]);
        fit_locate_to_budget(&mut answer, 5, text_tokens(7));
        assert_eq!(
            answer["limits"],
            serde_json::json!([
                "earlier limit",
                "the answer exceeds the budget by about 2 tokens without its dropped text"
            ])
        );
    }

    #[test]
    fn fit_locate_to_budget_should_call_an_answer_at_its_budget_within_it() {
        let mut answer = locate_answer(&["aaaa"]);
        fit_locate_to_budget(&mut answer, 9, text_tokens(5));
        assert_eq!(texts_of(&answer), ["aaaa"]);
        assert_eq!(answer["limits"], serde_json::json!(["earlier limit"]));
    }

    proptest::proptest! {
        #[test]
        fn fit_locate_to_budget_should_keep_a_ranked_prefix_and_drop_only_a_suffix(
            texts in proptest::collection::vec("[a-z]{0,16}", 0..8),
            overhead in 0usize..32,
            budget in 0usize..160,
        ) {
            let original: Vec<&str> = texts.iter().map(String::as_str).collect();
            let mut answer = locate_answer(&original);
            fit_locate_to_budget(&mut answer, budget, text_tokens(overhead));
            let kept = texts_of(&answer);

            let mut dropped = false;
            for (before, after) in original.iter().zip(kept) {
                if before.is_empty() {
                    prop_assert_eq!(after, *before);
                } else if after.is_empty() {
                    dropped = true;
                } else {
                    prop_assert!(!dropped, "a lower-ranked target survived after a context drop");
                    prop_assert_eq!(after, *before);
                }
            }

            if overhead <= budget {
                prop_assert!(text_tokens(overhead)(&answer) <= budget);
            }
        }
    }

    #[test]
    fn compiles_only_exact_auto_intents() {
        let result = compile_query("where is `login_user`?", QueryKind::Auto);
        assert_eq!(result.status, QueryStatus::Ranked);

        let result = compile_query("where is `login_user`", QueryKind::Auto);
        assert_eq!(result.status, QueryStatus::Resolved);
        assert_eq!(result.plan[0].recipe, "locate.v2");
        assert_eq!(result.plan[0].operations, LOCATE_OPERATIONS);
    }

    // Every V1 phrasing must compile to exactly its recipe: an agent that
    // types "show impact of X" must not be answered with a locate/scope
    // ranking, and each alternative in a two-phrase intent must resolve on
    // its own.
    #[test]
    fn each_auto_phrasing_resolves_to_its_recipe() {
        let cases = [
            (
                "what files implement checkout",
                "scope.v1",
                &["targets"][..],
            ),
            ("show impact of compile_query", "impact.v1", &["impact"][..]),
            (
                "restore the login form",
                "history_recovery.v1",
                &["excavate"][..],
            ),
            (
                "it was working before lunch",
                "history_recovery.v1",
                &["excavate"][..],
            ),
            ("status", "status.v1", &["inspect", "review", "changes"][..]),
            (
                "what changed",
                "status.v1",
                &["inspect", "review", "changes"][..],
            ),
        ];
        for (intent, recipe, operations) in cases {
            let result = compile_query(intent, QueryKind::Auto);
            assert_eq!(result.status, QueryStatus::Resolved, "{intent}");
            assert_eq!(result.plan.len(), 1, "{intent}");
            assert_eq!(result.plan[0].recipe, recipe, "{intent}");
            assert_eq!(result.plan[0].operations, operations, "{intent}");
        }
    }

    #[test]
    fn ambiguous_prose_is_ranked_not_resolved() {
        let result = compile_query("  explain the login flow ", QueryKind::Auto);
        assert_eq!(result.status, QueryStatus::Ranked);
        assert_eq!(result.intent, "explain the login flow");
        let recipes: Vec<&str> = result.plan.iter().map(|p| p.recipe.as_str()).collect();
        assert_eq!(recipes, ["locate.v2", "scope.v1"]);
    }

    #[test]
    fn explicit_kind_overrides_ambiguous_text() {
        let result = compile_query("explain login", QueryKind::Impact);
        assert_eq!(result.status, QueryStatus::Resolved);
        assert_eq!(result.plan[0].operations, ["impact"]);
    }

    fn candidate(path: &str, symbol: Option<&str>, score: f64) -> LocateCandidate {
        LocateCandidate {
            path: path.into(),
            start_line: 10,
            end_line: 20,
            symbol: symbol.map(str::to_owned),
            kind: None,
            score,
            reasons: Vec::new(),
        }
    }

    #[test]
    fn locate_status_should_claim_located_only_for_one_exact_fresh_symbol() {
        let one = [candidate("a.rs", Some("alpha"), 0.9)];
        assert_eq!(
            locate_status(Some("ident"), &one, true),
            LocateStatus::Located
        );
        assert_eq!(locate_status(Some("t0"), &one, true), LocateStatus::Located);
        let partial = [LocateCandidate {
            reasons: vec!["word overlap: thing".into()],
            ..candidate("a.rs", Some("alpha"), 0.4)
        }];
        assert_eq!(
            locate_status(Some("t2"), &partial, true),
            LocateStatus::NeedsSearch,
            "a partial word overlap is no lead"
        );
        let whole_phrase = [LocateCandidate {
            reasons: vec!["substring match".into()],
            ..candidate("a.rs", Some("alpha"), 0.8)
        }];
        assert_eq!(
            locate_status(Some("t2"), &whole_phrase, true),
            LocateStatus::NeedsReading
        );
        assert_eq!(
            locate_status(Some("t1"), &one, true),
            LocateStatus::NeedsReading
        );
        assert_eq!(
            locate_status(Some("t3"), &one, true),
            LocateStatus::NeedsSearch
        );
        assert_eq!(
            locate_status(Some("symbol"), &one, true),
            LocateStatus::NeedsSearch
        );
        assert_eq!(locate_status(None, &one, true), LocateStatus::NeedsSearch);
        // A weak tier is never ambiguous, however close the scores.
        let weak_tie = [
            candidate("a.rs", Some("alpha"), 0.5),
            candidate("b.rs", Some("beta"), 0.5),
        ];
        assert_eq!(
            locate_status(Some("t3"), &weak_tie, true),
            LocateStatus::NeedsSearch
        );
        assert_eq!(
            locate_status(Some("ident"), &one, false),
            LocateStatus::NeedsReading,
            "a stale source is not a located answer"
        );
        let outside = [candidate("a.rs", None, 0.9)];
        assert_eq!(
            locate_status(Some("t0"), &outside, true),
            LocateStatus::NeedsReading
        );
        assert_eq!(
            locate_status(Some("t0"), &[], true),
            LocateStatus::NeedsSearch
        );
    }

    #[test]
    fn locate_status_should_call_a_tie_in_another_place_ambiguous() {
        let best = candidate("a.rs", Some("alpha"), 0.5);
        // Exactly on the margin still ties.
        let tie_elsewhere = [best.clone(), candidate("b.rs", Some("alpha"), 0.4375)];
        assert_eq!(
            locate_status(Some("ident"), &tie_elsewhere, true),
            LocateStatus::Ambiguous
        );
        let other_symbol = [best.clone(), candidate("a.rs", Some("beta"), 0.5)];
        assert_eq!(
            locate_status(Some("ident"), &other_symbol, true),
            LocateStatus::Ambiguous
        );
        // A second match in the same symbol, or a clearly lower one, is no rival.
        let same_place = [best.clone(), candidate("a.rs", Some("alpha"), 0.5)];
        assert_eq!(
            locate_status(Some("ident"), &same_place, true),
            LocateStatus::Located
        );
        let distant = [best, candidate("b.rs", Some("alpha"), 0.375)];
        assert_eq!(
            locate_status(Some("ident"), &distant, true),
            LocateStatus::Located
        );
    }

    #[test]
    fn locate_targets_should_keep_distinct_symbols_best_first_up_to_the_cap() {
        let candidates = [
            candidate("a.rs", None, 0.95),
            candidate("a.rs", Some("alpha"), 0.9),
            candidate("a.rs", Some("alpha"), 0.8),
            candidate("b.rs", Some("alpha"), 0.7),
            candidate("c.rs", Some("gamma"), 0.6),
        ];
        let picked: Vec<(&str, Option<&str>)> = locate_targets(&candidates, 2)
            .iter()
            .map(|c| (c.path.as_str(), c.symbol.as_deref()))
            .collect();
        assert_eq!(picked, [("a.rs", Some("alpha")), ("b.rs", Some("alpha"))]);
        assert_eq!(locate_targets(&candidates, 3).len(), 3);
        assert!(locate_targets(&candidates, 0).is_empty());
    }

    #[test]
    fn split_context_budget_should_leave_a_tenth_for_the_rest() {
        assert_eq!(split_context_budget(1000, 3), [600, 150, 150]);
        assert_eq!(split_context_budget(1000, 1), [600]);
        assert_eq!(split_context_budget(799, 2), [479, 119]);
        assert!(split_context_budget(1000, 0).is_empty());
    }

    #[test]
    fn locate_candidates_should_read_symbol_and_owner_matches_in_order() {
        let resolve = serde_json::json!({"matches": [
            {"path": "a.rs", "start_line": 3, "end_line": 9, "raw": "alpha",
             "symbol_kind": "function", "owner": null, "score": 0.7,
             "reasons": ["exact symbol name match"]},
            {"path": "b.rs", "start_line": 12, "end_line": 12, "raw": "some text",
             "symbol_kind": null, "owner": "beta", "score": 0.5, "reasons": []},
            {"path": "c.rs", "start_line": 1, "end_line": 1, "raw": "loose",
             "symbol_kind": null, "owner": null, "score": 0.25}
        ]});
        assert_eq!(
            locate_candidates(&resolve),
            [
                LocateCandidate {
                    path: "a.rs".into(),
                    start_line: 3,
                    end_line: 9,
                    symbol: Some("alpha".into()),
                    kind: Some("function".into()),
                    score: 0.7,
                    reasons: vec!["exact symbol name match".into()],
                },
                candidate_at("b.rs", 12, 12, Some("beta"), 0.5),
                candidate_at("c.rs", 1, 1, None, 0.25),
            ]
        );
        assert!(locate_candidates(&serde_json::json!({})).is_empty());
    }

    fn candidate_at(
        path: &str,
        start: u32,
        end: u32,
        symbol: Option<&str>,
        score: f64,
    ) -> LocateCandidate {
        LocateCandidate {
            start_line: start,
            end_line: end,
            ..candidate(path, symbol, score)
        }
    }

    #[test]
    fn candidate_uid_in_should_pick_the_homonym_in_the_given_file() {
        let response = serde_json::json!({"candidates": [
            {"uid": "a.rs#render#function", "path": "a.rs"},
            {"uid": "b.rs#render#method", "path": "b.rs"}
        ]});
        assert_eq!(
            candidate_uid_in(&response, "b.rs").as_deref(),
            Some("b.rs#render#method")
        );
        assert_eq!(candidate_uid_in(&response, "c.rs"), None);
        assert_eq!(candidate_uid_in(&serde_json::json!({}), "a.rs"), None);
    }

    #[test]
    fn looks_like_test_path_should_follow_the_usual_conventions() {
        for path in [
            "crates/x/tests/cli.rs",
            "test/a.js",
            "src/__tests__/a.ts",
            "spec/a_spec.rb",
            "pkg/test_util.py",
            "pkg/handler_test.go",
            "src/a.test.ts",
            "src/a.spec.ts",
        ] {
            assert!(looks_like_test_path(path), "{path}");
        }
        for path in [
            "src/testing.rs",
            "src/contest.rs",
            "src/attest/a.rs",
            "latest.rs",
        ] {
            assert!(!looks_like_test_path(path), "{path}");
        }
    }

    #[test]
    fn caller_test_files_should_list_each_test_file_once_sorted() {
        let uses = serde_json::json!({"edges": [
            {"symbol": {"path": "tests/b.rs"}},
            {"symbol": {"path": "src/lib.rs"}},
            {"symbol": {"path": "tests/a.rs"}},
            {"symbol": {"path": "tests/b.rs"}}
        ]});
        assert_eq!(caller_test_files(&uses), ["tests/a.rs", "tests/b.rs"]);
        assert!(caller_test_files(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn same_snapshot_should_flag_responses_from_different_trees() {
        let a = serde_json::json!({"snapshot": {"head": "1", "dirty_count": 0}});
        let b = serde_json::json!({"snapshot": {"head": "1", "dirty_count": 0}});
        let c = serde_json::json!({"snapshot": {"head": "1", "dirty_count": 2}});
        let none = serde_json::json!({});
        assert!(same_snapshot(&[&a, &b, &none]));
        assert!(!same_snapshot(&[&a, &c]));
        assert!(same_snapshot(&[&none]));
    }

    #[test]
    fn locate_next_action_should_name_a_concrete_step_per_status() {
        let best = candidate("src/a.rs", Some("alpha"), 0.5);
        let uids = [
            "a.rs#alpha#function".to_owned(),
            "b.rs#alpha#method".to_owned(),
        ];
        assert_eq!(
            locate_next_action(
                LocateStatus::Located,
                "alpha",
                "where is `alpha`",
                &uids,
                Some(&best)
            ),
            None
        );
        assert_eq!(
            locate_next_action(LocateStatus::Ambiguous, "alpha", "i", &uids, Some(&best))
                .as_deref(),
            Some("pick the intended candidate by uid, e.g. pixel pack-context 'b.rs#alpha#method'")
        );
        assert_eq!(
            locate_next_action(LocateStatus::Ambiguous, "alpha", "i", &uids[..1], None).as_deref(),
            Some("pixel find-code 'alpha'")
        );
        assert_eq!(
            locate_next_action(LocateStatus::NeedsReading, "alpha", "i", &[], Some(&best))
                .as_deref(),
            Some("read src/a.rs:10-20 before relying on it")
        );
        assert_eq!(
            locate_next_action(LocateStatus::NeedsReading, "alpha", "i", &[], None),
            None
        );
        assert_eq!(
            locate_next_action(LocateStatus::NeedsSearch, "it's_here", "i", &[], None).as_deref(),
            Some("pixel search-content -F 'it'\\''s_here'")
        );
        assert_eq!(
            locate_next_action(
                LocateStatus::NeedsSearch,
                "retry push",
                "retry a push",
                &[],
                None
            )
            .as_deref(),
            Some("pixel scope-task 'retry a push' --no-manifest")
        );
    }

    #[test]
    fn likely_uid_should_need_both_a_symbol_and_its_kind() {
        let symbol_match = LocateCandidate {
            kind: Some("function".into()),
            ..candidate("src/a.rs", Some("alpha"), 0.5)
        };
        assert_eq!(
            symbol_match.likely_uid().as_deref(),
            Some("src/a.rs#alpha#function")
        );
        assert_eq!(candidate("src/a.rs", Some("alpha"), 0.5).likely_uid(), None);
        let no_symbol = LocateCandidate {
            kind: Some("function".into()),
            ..candidate("src/a.rs", None, 0.5)
        };
        assert_eq!(no_symbol.likely_uid(), None);
    }

    #[test]
    fn context_is_stale_should_read_the_cap_not_the_empty_text() {
        let stale = serde_json::json!({"text": "", "caps": [
            "context truncated: source differs from graph snapshot or is unavailable; stale excerpts and Crux omitted"
        ]});
        assert!(context_is_stale(&stale));
        assert!(!context_is_stale(&serde_json::json!({"text": ""})));
        assert!(!context_is_stale(
            &serde_json::json!({"text": "", "caps": ["edge cap"]})
        ));
    }
}
