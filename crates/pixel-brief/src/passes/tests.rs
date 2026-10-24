// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

fn candidate(head: &str, lines: &[(usize, &str)], part: Part, order: usize) -> Candidate {
    Candidate {
        priority: order,
        order: (1, order, 0),
        part,
        block: Block::new(
            head.to_string(),
            lines
                .iter()
                .map(|(n, text)| format!("{n}| {text}"))
                .collect(),
        ),
    }
}

fn heads(candidates: &[Candidate]) -> Vec<&str> {
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by_key(|candidate| candidate.order);
    sorted
        .into_iter()
        .map(|candidate| candidate.block.head())
        .collect()
}

fn passes(prompt: &str) -> Passes {
    Passes {
        rank: true,
        prune: true,
        prompt: prompt.to_string(),
        ..Passes::default()
    }
}

#[test]
fn blocks_that_share_no_word_with_the_question_go_and_the_rest_rank_by_score() {
    let candidates = vec![
        candidate(
            "src/lib.rs:1-3 — unrelated definition (function)",
            &[(1, "fn a() {}")],
            Part::Definition,
            0,
        ),
        candidate(
            "src/cache.rs:1-3 — evict definition (function)",
            &[(1, "fn evict() {}")],
            Part::Definition,
            1,
        ),
        candidate(
            "src/cache.rs:10-12 — evict usage in cache policy",
            &[(10, "evict();")],
            Part::Usage,
            2,
        ),
    ];
    let ranked = rank_blocks(candidates, &[], "how does the cache policy evict entries");
    assert_eq!(
        heads(&ranked),
        [
            "src/cache.rs:10-12 — evict usage in cache policy",
            "src/cache.rs:1-3 — evict definition (function)",
        ]
    );
}

#[test]
fn the_pack_is_never_emptied_and_the_steps_own_blocks_are_never_touched() {
    let candidates = vec![
        candidate(
            "src/lib.rs:1-3 — unrelated definition (function)",
            &[(1, "fn a() {}")],
            Part::Definition,
            0,
        ),
        candidate("workspace — 3 members (cargo)", &[], Part::Locator, 1),
    ];
    let own = vec!["workspace — 3 members (cargo)".to_string()];
    let ranked = rank_blocks(
        candidates.clone(),
        &own,
        "how does the cache policy evict entries",
    );
    assert_eq!(ranked.len(), 2, "no block shares a word: nothing goes");
    let with_hit = vec![
        candidate(
            "src/cache.rs:1-3 — evict definition",
            &[(1, "x")],
            Part::Definition,
            0,
        ),
        candidate("src/lib.rs:1-3 — a", &[(1, "x")], Part::Definition, 1),
        candidate("workspace — 3 members (cargo)", &[], Part::Locator, 2),
    ];
    let ranked = rank_blocks(with_hit, &own, "how does evict work");
    assert_eq!(
        heads(&ranked),
        [
            "src/cache.rs:1-3 — evict definition",
            "workspace — 3 members (cargo)"
        ]
    );
}

#[test]
fn a_list_of_test_names_goes_when_a_test_body_is_shown_and_completions_go_unless_asked() {
    let body = candidate(
        "tests/a.rs:1-3 — evicts_the_oldest (a test of evict)",
        &[(1, "#[test]"), (2, "fn evicts_the_oldest() {}")],
        Part::TestBody,
        0,
    );
    let list = candidate("tests of evict", &[], Part::Tests, 1);
    let zsh = candidate(
        "completions/_rg:1-3 — rg region",
        &[(1, "#compdef rg")],
        Part::Definition,
        2,
    );
    let kept = prune_noise(
        vec![body.clone(), list.clone(), zsh.clone()],
        &[],
        &passes("what tests cover evict"),
    );
    assert_eq!(
        heads(&kept),
        ["tests/a.rs:1-3 — evicts_the_oldest (a test of evict)"]
    );
    let without_body = prune_noise(vec![list.clone()], &[], &passes("what tests cover evict"));
    assert_eq!(
        heads(&without_body),
        ["tests of evict"],
        "a list alone is all there is"
    );
    let asked = prune_noise(
        vec![zsh.clone()],
        &[],
        &passes("how are shell completions generated"),
    );
    assert_eq!(asked.len(), 1);
}

#[test]
fn overlapping_ranges_of_one_file_join_into_the_first_block() {
    let first = candidate(
        "src/a.rs:1-3 — f definition (function)",
        &[(1, "fn f() {"), (2, "    g();"), (3, "}")],
        Part::Definition,
        0,
    );
    let second = candidate(
        "src/a.rs:3-5 — f usage in h",
        &[(3, "}"), (4, "fn h() {"), (5, "    f();")],
        Part::Usage,
        1,
    );
    let other_file = candidate("src/b.rs:1-1 — x", &[(1, "x")], Part::Usage, 2);
    let kept = prune_noise(
        vec![first, second, other_file],
        &[],
        &passes("what does f do"),
    );
    assert_eq!(kept.len(), 2);
    assert_eq!(
        kept[0].block.head(),
        "src/a.rs:1-5 — f definition (function)"
    );
    assert_eq!(
        kept[0].block.body(),
        [
            "1| fn f() {",
            "2|     g();",
            "3| }",
            "4| fn h() {",
            "5|     f();"
        ]
    );
}

#[test]
fn a_region_loses_the_lines_another_block_of_its_file_shows() {
    let region_lines: Vec<(usize, &str)> = (1..=12).map(|n| (n, "line")).collect();
    let region = candidate(
        "src/a.rs:1-12 — a.rs region",
        &region_lines,
        Part::Definition,
        0,
    );
    let shown = candidate(
        "src/a.rs:5-6 — f definition (function)",
        &[(5, "fn f() {"), (6, "}")],
        Part::Definition,
        1,
    );
    let kept = prune_noise(vec![region, shown], &[], &passes("what does f do"));
    let heads: Vec<&str> = kept.iter().map(|c| c.block.head()).collect();
    // 1-4 and 7-12 remain (runs of at least three lines).
    assert_eq!(
        heads,
        [
            "src/a.rs:1-4 — a.rs region",
            "src/a.rs:7-12 — a.rs region",
            "src/a.rs:5-6 — f definition (function)",
        ]
    );
}

#[test]
fn a_block_above_the_cap_keeps_the_window_with_the_most_question_words() {
    let lines: Vec<(usize, String)> = (1..=150)
        .map(|n| {
            (
                n,
                if n == 100 {
                    "let evicted = cache.evict_oldest(policy);".to_string()
                } else {
                    format!("let filler_{n} = {n}; // padding padding")
                },
            )
        })
        .collect();
    let borrowed: Vec<(usize, &str)> = lines.iter().map(|(n, t)| (*n, t.as_str())).collect();
    let big = candidate(
        "src/a.rs:1-150 — evict_oldest region",
        &borrowed,
        Part::Definition,
        0,
    );
    let mut capped = vec![big];
    cap_blocks(
        &mut capped,
        &[],
        &passes("how does evict_oldest apply the cache policy"),
    );
    let ranged = Ranged::parse(&capped[0].block).unwrap();
    assert!(ranged.bytes() <= BLOCK_BYTES, "{}", ranged.bytes());
    assert!(ranged.lines.contains_key(&100) && ranged.lines.len() < 150);
    assert_eq!(
        capped[0].block.head(),
        format!(
            "src/a.rs:{}-{} — evict_oldest region",
            ranged.lo(),
            ranged.hi()
        )
    );
}

#[test]
fn a_long_test_keeps_eighteen_lines_around_its_assertions() {
    let lines: Vec<(usize, String)> = (1..=40)
        .map(|n| {
            (
                n,
                if n == 30 {
                    "    assert_eq!(evict(), 1);".to_string()
                } else {
                    format!("    step({n});")
                },
            )
        })
        .collect();
    let borrowed: Vec<(usize, &str)> = lines.iter().map(|(n, t)| (*n, t.as_str())).collect();
    let test = candidate(
        "tests/a.rs:1-40 — it (a test of evict)",
        &borrowed,
        Part::TestBody,
        0,
    );
    let mut capped = vec![test];
    cap_blocks(&mut capped, &[], &passes("what tests cover evict"));
    let ranged = Ranged::parse(&capped[0].block).unwrap();
    assert_eq!(ranged.lines.len(), TEST_LINES);
    assert!(ranged.lines.contains_key(&30));
}

#[test]
fn words_and_squashing_follow_the_prototype() {
    assert_eq!(
        words("Where is HTTPServerError defined"),
        BTreeSet::from(["http".into(), "server".into(), "error".into()])
    );
    assert_eq!(squash("Max-Body_Size 2"), "maxbodysize2");
    let anchors = Anchors::new(&Passes {
        prompt: "why does evict_oldest skip the --dry-run flag".into(),
        names: vec!["Cache".into()],
        ..Passes::default()
    });
    assert!(anchors.symbols.contains("evictoldest") && anchors.symbols.contains("cache"));
    // Two plain words (evict, oldest) and two named symbols (evict_oldest, cache).
    assert_eq!(
        anchors.weight("self.cache.evict_oldest()", None),
        2 + 2 * SYMBOL_WEIGHT
    );
}
