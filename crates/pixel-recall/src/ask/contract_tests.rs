// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for `recall ask`: what the semantic channel contributes,
//! which filters it honours, how the lexical budget is spent, and what a
//! snippet shows.

use super::*;
use crate::model::{IntentSource, Role};
use crate::testutil::{TS, add_session, add_session_with_intents};

/// An embedder that answers every text with the same unit vector, so every
/// stored chunk is an equally good semantic match.
struct FlatEmbedder;

impl Embedder for FlatEmbedder {
    fn model_id(&self) -> &str {
        crate::embed::POTION_REPO
    }
    fn dims(&self) -> usize {
        3
    }
    fn embed_batch(&mut self, texts: &[&str], _kind: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
}

/// An embedder bound to another model than the store.
struct OtherModel;

impl Embedder for OtherModel {
    fn model_id(&self) -> &str {
        "other/model"
    }
    fn dims(&self) -> usize {
        3
    }
    fn embed_batch(&mut self, texts: &[&str], _kind: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    store: RecallStore,
    segments: SegmentSet,
    vectors: VectorStore,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        let segments = SegmentSet::open(&tmp.path().join("segments")).unwrap();
        let vectors = VectorStore::open(&tmp.path().join("vectors")).unwrap();
        Self {
            _tmp: tmp,
            store,
            segments,
            vectors,
        }
    }

    fn turn_ids(&self, session_id: i64) -> Vec<i64> {
        let mut stmt = self
            .store
            .connection()
            .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map([session_id], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Give `turn_id` one chunk starting at `start`, with the shared vector.
    fn embed_turn(&mut self, turn_id: i64, start: usize) {
        let ids = self
            .store
            .insert_chunks(turn_id, &[(start, start + 1)])
            .unwrap();
        self.vectors
            .append_segment(
                crate::embed::POTION_REPO,
                3,
                &[(ids[0], vec![1.0, 0.0, 0.0])],
            )
            .unwrap();
    }

    fn index(&mut self) {
        self.segments.index_new(&self.store).unwrap();
    }

    fn ask(
        &self,
        embedder: Option<&mut (dyn Embedder + 'static)>,
        query: &str,
        filters: &SearchFilters,
        k: usize,
    ) -> Result<AskResult, String> {
        ask(
            &self.store,
            &self.segments,
            &self.vectors,
            embedder,
            query,
            filters,
            k,
            false,
        )
    }
}

// --- semantic channel -----------------------------------------------------

#[test]
fn ask_should_answer_from_the_semantic_channel_when_no_query_word_matches() {
    let mut fx = Fixture::new();
    let session = add_session(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[(Role::Assistant, "rotate the unix socket after a restart")],
    );
    let turn = fx.turn_ids(session)[0];
    fx.embed_turn(turn, 7);
    fx.index();

    let result = fx
        .ask(
            Some(&mut FlatEmbedder),
            "zzqqxx",
            &SearchFilters::default(),
            5,
        )
        .unwrap();
    assert_eq!(result.notice, None, "the semantic channel ran");
    assert_eq!(result.groups.len(), 1, "{:?}", result.groups);
    let hit = &result.groups[0].best;
    assert_eq!(hit.turn_id, turn);
    assert!(hit.matched_semantic);
    assert!(!hit.matched_lexical);
    // A semantic-only hit shows the head of its chunk, not of the turn.
    assert_eq!(hit.snippet, "the unix socket after a restart");
}

#[test]
fn ask_should_mark_both_channels_when_a_turn_matches_lexically_and_semantically() {
    let mut fx = Fixture::new();
    let session = add_session(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[(Role::Assistant, "the watchdog restarts the daemon")],
    );
    let turn = fx.turn_ids(session)[0];
    fx.embed_turn(turn, 0);
    fx.index();

    let result = fx
        .ask(
            Some(&mut FlatEmbedder),
            "watchdog",
            &SearchFilters::default(),
            5,
        )
        .unwrap();
    let hit = &result.groups[0].best;
    assert!(hit.matched_lexical && hit.matched_semantic, "{hit:?}");
}

#[test]
fn ask_should_refuse_a_vector_store_built_with_another_model() {
    let mut fx = Fixture::new();
    let session = add_session(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[(Role::Assistant, "x")],
    );
    let turn = fx.turn_ids(session)[0];
    fx.embed_turn(turn, 0);
    fx.index();

    let err = fx
        .ask(
            Some(&mut OtherModel),
            "anything",
            &SearchFilters::default(),
            5,
        )
        .unwrap_err();
    assert!(err.contains("recall embed --rebuild"), "{err}");
}

#[test]
fn ask_should_skip_a_chunk_whose_turn_no_longer_exists() {
    let mut fx = Fixture::new();
    // A chunk left behind by a re-ingested turn: its turn id resolves to no row.
    fx.embed_turn(9_999, 0);
    fx.index();
    let result = fx
        .ask(
            Some(&mut FlatEmbedder),
            "anything",
            &SearchFilters::default(),
            5,
        )
        .unwrap();
    assert!(result.groups.is_empty(), "{:?}", result.groups);
    assert_eq!(result.notice, None);
}

/// Every metadata filter, set alone, must also restrict the semantic channel:
/// a filtered `ask` that let KNN return any chunk would answer from sessions
/// the caller excluded.
#[test]
fn ask_should_apply_each_filter_alone_to_the_semantic_channel() {
    let mut fx = Fixture::new();
    // A harness-injected user turn (orchestrator intent), at `TS`, in
    // /work/pixel, written by claude.
    let session = add_session_with_intents(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[(Role::User, Some(IntentSource::Orchestrator), "alpha text")],
    );
    let turn = fx.turn_ids(session)[0];
    fx.embed_turn(turn, 0);
    fx.index();

    let unfiltered = fx
        .ask(
            Some(&mut FlatEmbedder),
            "zzqqxx",
            &SearchFilters::default(),
            5,
        )
        .unwrap();
    assert_eq!(
        unfiltered.groups.len(),
        1,
        "the fixture is reachable unfiltered"
    );

    let excluding: Vec<(&str, SearchFilters)> = vec![
        (
            "agent",
            SearchFilters {
                agent: Some("codex".into()),
                ..SearchFilters::default()
            },
        ),
        (
            "repo_prefix",
            SearchFilters {
                repo_prefix: Some("/elsewhere".into()),
                ..SearchFilters::default()
            },
        ),
        (
            "since_ms",
            SearchFilters {
                since_ms: Some(TS + 1),
                ..SearchFilters::default()
            },
        ),
        (
            "until_ms",
            SearchFilters {
                until_ms: Some(TS - 1),
                ..SearchFilters::default()
            },
        ),
        (
            "role",
            SearchFilters {
                role: Some("assistant".into()),
                ..SearchFilters::default()
            },
        ),
        (
            "human_only",
            SearchFilters {
                human_only: true,
                ..SearchFilters::default()
            },
        ),
        (
            "session_id",
            SearchFilters {
                session_id: Some(session + 1),
                ..SearchFilters::default()
            },
        ),
    ];
    for (name, filters) in excluding {
        let result = fx
            .ask(Some(&mut FlatEmbedder), "zzqqxx", &filters, 5)
            .unwrap();
        assert!(
            result.groups.is_empty(),
            "filter {name} must exclude the turn: {:?}",
            result.groups
        );
    }
}

// --- grouping and k -------------------------------------------------------

#[test]
fn ask_should_return_at_most_k_sessions_and_count_the_rest_of_a_session_as_extra() {
    let mut fx = Fixture::new();
    add_session(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[
            (Role::Assistant, "the flamingo nests here"),
            (Role::Assistant, "another flamingo turn"),
        ],
    );
    add_session(
        &mut fx.store,
        "codex",
        "bbbb2222",
        &[(Role::Assistant, "a flamingo elsewhere")],
    );
    fx.index();
    let result = fx
        .ask(None, "flamingo", &SearchFilters::default(), 1)
        .unwrap();
    assert_eq!(result.groups.len(), 1, "{:?}", result.groups);

    let both = fx
        .ask(None, "flamingo", &SearchFilters::default(), 5)
        .unwrap();
    let mut extras: Vec<(String, usize)> = both
        .groups
        .iter()
        .map(|g| (g.best.source_session_id.clone(), g.extra_hits))
        .collect();
    extras.sort();
    assert_eq!(
        extras,
        vec![("aaaa1111".to_string(), 1), ("bbbb2222".to_string(), 0)]
    );
}

/// Words the index cannot cost force a full scan each; only two are spent
/// per question, chosen alphabetically among equals, so a vague query
/// cannot walk the corpus once per short word.
#[test]
fn ask_should_search_at_most_two_uncostable_words() {
    let mut fx = Fixture::new();
    add_session(
        &mut fx.store,
        "claude",
        "aaaa1111",
        &[(Role::Assistant, "always run rg first")],
    );
    add_session(
        &mut fx.store,
        "codex",
        "bbbb2222",
        &[(Role::Assistant, "never cd anywhere")],
    );
    add_session(
        &mut fx.store,
        "pi",
        "cccc3333",
        &[(Role::Assistant, "then ls the folder")],
    );
    fx.index();
    let result = fx
        .ask(None, "rg ls cd", &SearchFilters::default(), 5)
        .unwrap();
    let mut sessions: Vec<&str> = result
        .groups
        .iter()
        .map(|g| g.best.source_session_id.as_str())
        .collect();
    sessions.sort_unstable();
    assert_eq!(
        sessions,
        vec!["bbbb2222", "cccc3333"],
        "`cd` and `ls` are searched, `rg` sorts third and is dropped"
    );
}

// --- tokenization and patterns --------------------------------------------

#[test]
fn query_words_should_trim_punctuation_and_dedupe_case_insensitively() {
    assert_eq!(
        query_words("--verbose. Cargo cargo CARGO IN a.b"),
        vec!["verbose", "Cargo", "a.b"]
    );
}

#[test]
fn word_pattern_should_match_the_common_casings_of_a_word_as_a_whole_word() {
    assert_eq!(word_pattern("Foo"), r"\b(?:FOO|Foo|foo)\b");
    assert_eq!(word_pattern("a.b"), r"\b(?:A\.B|A\.b|a\.b)\b");
}

// --- snippets ---------------------------------------------------------------

#[test]
fn make_snippet_should_center_on_the_first_query_word_match() {
    let re = regex::Regex::new(r"\bneedle\b").unwrap();
    let text = "hay hay needle hay";
    let expected = crate::search::snippet_around(text, 8, 14).0;
    assert_eq!(make_snippet(text, Some(&re), Some(3)), expected);
}

#[test]
fn make_snippet_should_show_the_chunk_head_when_no_word_matches() {
    let re = regex::Regex::new(r"\babsent\b").unwrap();
    assert_eq!(
        make_snippet("line one\nline two", Some(&re), Some(5)),
        "one line two"
    );
    assert_eq!(make_snippet("short text", None, None), "short text");
}

#[test]
fn make_snippet_should_cut_at_160_bytes_with_an_ellipsis() {
    let text = "x".repeat(200);
    let snippet = make_snippet(&text, None, Some(0));
    assert_eq!(snippet, format!("{}…", "x".repeat(160)));
    assert_eq!(make_snippet(&"y".repeat(160), None, None), "y".repeat(160));
}

#[test]
fn make_snippet_should_clamp_an_out_of_range_chunk_start() {
    assert_eq!(make_snippet("abc", None, Some(99)), "");
    assert_eq!(make_snippet("abc", None, Some(-5)), "abc");
}

#[test]
fn make_snippet_should_never_split_a_multibyte_character() {
    // "é" is two bytes: a start inside it backs off to its first byte.
    assert_eq!(make_snippet("aébc", None, Some(2)), "ébc");
    // An end inside a multibyte character advances past it.
    let text = format!("{}é tail", "z".repeat(159));
    let snippet = make_snippet(&text, None, Some(0));
    assert_eq!(snippet, format!("{}é…", "z".repeat(159)));
}
