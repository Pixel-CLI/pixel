// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Query planning: regex pattern → boolean gram query.
//!
//! A simplified form of Russ Cox's trigram-query algebra (regexp4), with
//! sparse-gram coverings at the leaves. The plan is *sound, never complete*:
//! it may under-narrow (worst case `All` = full scan) but a document matching
//! the regex is always in the candidate set, because a requirement is only
//! emitted for literals that must appear verbatim in any match.
//!
//! MVP scope: exact-literal tracking through concat/alternation/captures,
//! small-class expansion (so ASCII case-insensitive literals still narrow),
//! required-literal extraction from `min >= 1` repetitions. Prefix/suffix
//! set tracking from the full Cox algebra is a later refinement.

use regex_syntax::hir::{Class, Hir, HirKind};

use crate::gram::GramExtractor;
use crate::posting::GramQuery;

/// Cap on the number of alternative literals tracked per node before the
/// exact set is collapsed into a requirement (Cox's information-discarding
/// trim that keeps `[a-z]{4}`-style patterns from exploding).
const MAX_EXACT: usize = 64;
/// Classes up to this many single-byte members expand into alternatives.
const MAX_CLASS_EXPANSION: usize = 4;

#[derive(Debug)]
enum Info {
    /// Node matches exactly one of these byte strings (complete set).
    Exact(Vec<Vec<u8>>),
    /// Boolean gram requirement that any match implies.
    Required(GramQuery),
}

pub fn plan_pattern(
    pattern: &str,
    extractor: &dyn GramExtractor,
) -> Result<GramQuery, Box<regex_syntax::Error>> {
    let hir = regex_syntax::Parser::new().parse(pattern)?;
    let info = analyze(&hir, extractor);
    Ok(finish(info, extractor))
}

/// Plan for a fixed literal string (no regex semantics).
pub fn plan_literal(literal: &[u8], extractor: &dyn GramExtractor) -> GramQuery {
    covering_query(literal, extractor)
}

fn covering_query(literal: &[u8], extractor: &dyn GramExtractor) -> GramQuery {
    let grams = extractor.covering(literal);
    if grams.is_empty() {
        GramQuery::All
    } else {
        GramQuery::And(grams.into_iter().map(GramQuery::Literal).collect())
    }
}

/// OR over the covering-queries of a complete alternative set. If any
/// alternative cannot narrow, the whole set cannot.
fn exact_to_query(alts: &[Vec<u8>], extractor: &dyn GramExtractor) -> GramQuery {
    let mut branches = Vec::with_capacity(alts.len());
    for alt in alts {
        match covering_query(alt, extractor) {
            GramQuery::All => return GramQuery::All,
            q => branches.push(q),
        }
    }
    match branches.len() {
        0 => GramQuery::All,
        1 => branches.pop().unwrap(),
        _ => GramQuery::Or(branches),
    }
}

fn finish(info: Info, extractor: &dyn GramExtractor) -> GramQuery {
    match info {
        Info::Exact(alts) => exact_to_query(&alts, extractor),
        Info::Required(q) => simplify(q),
    }
}

fn and(mut parts: Vec<GramQuery>) -> GramQuery {
    parts.retain(|p| !matches!(p, GramQuery::All));
    match parts.len() {
        0 => GramQuery::All,
        1 => parts.pop().unwrap(),
        _ => GramQuery::And(parts),
    }
}

fn simplify(q: GramQuery) -> GramQuery {
    match q {
        GramQuery::And(children) => {
            let mut flat = Vec::new();
            for c in children {
                match simplify(c) {
                    GramQuery::All => {}
                    GramQuery::And(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            and(flat)
        }
        GramQuery::Or(children) => {
            let mut flat = Vec::new();
            for c in children {
                match simplify(c) {
                    // OR with an un-narrowable branch is un-narrowable.
                    GramQuery::All => return GramQuery::All,
                    GramQuery::Or(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            match flat.len() {
                0 => GramQuery::All,
                1 => flat.pop().unwrap(),
                _ => GramQuery::Or(flat),
            }
        }
        other => other,
    }
}

fn class_alternatives(class: &Class) -> Option<Vec<Vec<u8>>> {
    match class {
        Class::Bytes(b) => {
            let mut alts = Vec::new();
            for range in b.ranges() {
                let (lo, hi) = (range.start(), range.end());
                if usize::from(hi - lo) + alts.len() > MAX_CLASS_EXPANSION {
                    return None;
                }
                for byte in lo..=hi {
                    alts.push(vec![byte]);
                }
            }
            (!alts.is_empty()).then_some(alts)
        }
        Class::Unicode(u) => {
            let mut alts: Vec<Vec<u8>> = Vec::new();
            for range in u.ranges() {
                let (lo, hi) = (range.start() as u32, range.end() as u32);
                if (hi - lo) as usize + alts.len() > MAX_CLASS_EXPANSION {
                    return None;
                }
                for cp in lo..=hi {
                    let ch = char::from_u32(cp)?;
                    let mut buf = [0u8; 4];
                    alts.push(ch.encode_utf8(&mut buf).as_bytes().to_vec());
                }
            }
            (!alts.is_empty()).then_some(alts)
        }
    }
}

fn analyze(hir: &Hir, extractor: &dyn GramExtractor) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::Exact(vec![Vec::new()]),
        HirKind::Literal(lit) => Info::Exact(vec![lit.0.to_vec()]),
        HirKind::Class(class) => match class_alternatives(class) {
            Some(alts) => Info::Exact(alts),
            None => Info::Required(GramQuery::All),
        },
        HirKind::Capture(cap) => analyze(&cap.sub, extractor),
        HirKind::Repetition(rep) => {
            if rep.min == 0 {
                // Optional: contributes nothing mandatory.
                Info::Required(GramQuery::All)
            } else if rep.min == 1 && rep.max == Some(1) {
                analyze(&rep.sub, extractor)
            } else {
                // Occurs at least once; its requirement holds, but the
                // exact set does not survive repetition.
                let info = analyze(&rep.sub, extractor);
                Info::Required(finish(info, extractor))
            }
        }
        HirKind::Alternation(subs) => {
            let mut exact_union: Option<Vec<Vec<u8>>> = Some(Vec::new());
            let mut branch_queries = Vec::with_capacity(subs.len());
            for sub in subs {
                let info = analyze(sub, extractor);
                if let (Some(union), Info::Exact(alts)) = (&mut exact_union, &info) {
                    if union.len() + alts.len() <= MAX_EXACT {
                        union.extend(alts.iter().cloned());
                    } else {
                        exact_union = None;
                    }
                } else {
                    exact_union = None;
                }
                branch_queries.push(finish(info, extractor));
            }
            if let Some(union) = exact_union {
                Info::Exact(union)
            } else {
                Info::Required(simplify(GramQuery::Or(branch_queries)))
            }
        }
        HirKind::Concat(subs) => {
            // Fold children left to right: consecutive exact sets combine by
            // cross-product (capped); a non-exact child flushes the run into
            // an ANDed requirement.
            let mut requirements: Vec<GramQuery> = Vec::new();
            let mut run: Vec<Vec<u8>> = vec![Vec::new()];
            let mut run_complete_from_start = true;
            let flush =
                |run: &mut Vec<Vec<u8>>, requirements: &mut Vec<GramQuery>, complete: bool| {
                    if !(run.len() == 1 && run[0].is_empty()) {
                        requirements.push(exact_to_query(run, extractor));
                    }
                    *run = vec![Vec::new()];
                    let _ = complete;
                };
            let mut whole_exact = true;
            for sub in subs {
                match analyze(sub, extractor) {
                    Info::Exact(alts) => {
                        if run.len() * alts.len() <= MAX_EXACT {
                            let mut next = Vec::with_capacity(run.len() * alts.len());
                            for prefix in &run {
                                for alt in &alts {
                                    let mut s = prefix.clone();
                                    s.extend_from_slice(alt);
                                    next.push(s);
                                }
                            }
                            run = next;
                        } else {
                            // Trim: flush current run, restart from these
                            // alternatives alone. Requirement-only from here.
                            flush(&mut run, &mut requirements, run_complete_from_start);
                            whole_exact = false;
                            run = alts;
                        }
                    }
                    Info::Required(q) => {
                        whole_exact = false;
                        flush(&mut run, &mut requirements, run_complete_from_start);
                        run_complete_from_start = false;
                        if !matches!(q, GramQuery::All) {
                            requirements.push(q);
                        }
                    }
                }
            }
            if whole_exact {
                Info::Exact(run)
            } else {
                flush(&mut run, &mut requirements, run_complete_from_start);
                Info::Required(simplify(GramQuery::And(requirements)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gram::SparseGramExtractor;
    use crate::posting::resolve_query;
    use crate::weights::Crc32Weigher;
    use std::collections::HashMap;

    fn ex() -> SparseGramExtractor<Crc32Weigher> {
        SparseGramExtractor::new(Crc32Weigher)
    }

    fn plan(pattern: &str) -> GramQuery {
        plan_pattern(pattern, &ex()).unwrap()
    }

    #[test]
    fn plain_literal_narrows() {
        assert!(matches!(
            plan("handleClick"),
            GramQuery::And(_) | GramQuery::Literal(_)
        ));
    }

    #[test]
    fn short_literal_cannot_narrow() {
        assert_eq!(plan("ab"), GramQuery::All);
    }

    #[test]
    fn alternation_of_literals() {
        let q = plan("handleClick|openMenu");
        assert!(matches!(q, GramQuery::Or(_)));
    }

    #[test]
    fn alternation_with_short_branch_falls_back() {
        assert_eq!(plan("handleClick|ab"), GramQuery::All);
    }

    #[test]
    fn concat_with_class_keeps_both_sides() {
        // Both "fnـ" (with space) and "_test" must appear.
        let q = plan(r"fn \w+_test");
        match q {
            GramQuery::And(parts) => assert!(parts.len() >= 2),
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn optional_group_contributes_nothing() {
        // "(abc)?def" only requires "def".
        let q = plan("(abcdef)?ghijkl");
        let must = plan("ghijkl");
        assert_eq!(q, must);
    }

    #[test]
    fn repetition_min_one_keeps_requirement() {
        let q = plan("(handleClick)+");
        assert_ne!(q, GramQuery::All);
    }

    #[test]
    fn case_insensitive_ascii_still_narrows() {
        let q = plan("(?i)error");
        assert!(
            q != GramQuery::All,
            "small-class expansion should narrow (?i)"
        );
    }

    /// Soundness: every document the regex matches must be a candidate.
    #[test]
    fn candidates_never_miss_matching_docs() {
        let ex = ex();
        let docs: Vec<&[u8]> = vec![
            b"fn handle_test() {}",
            b"const MAX_FILE_SIZE: usize = 10;",
            b"handleClick(); openMenu();",
            b"nothing to see",
            b"ERROR: something broke",
            b"error in lowercase",
            b"fn tiny_test2() {}",
            b"abcdefghijkl",
            b"ghijkl only",
        ];
        // Inverted index over the docs.
        let mut inv: HashMap<u64, Vec<u32>> = HashMap::new();
        for (i, d) in docs.iter().enumerate() {
            let mut hits = Vec::new();
            ex.grams(d, &mut hits);
            let mut hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
            hashes.sort_unstable();
            hashes.dedup();
            for h in hashes {
                inv.entry(h).or_default().push(i as u32);
            }
        }
        let lookup = |h: u64| inv.get(&h).cloned().unwrap_or_default();

        for pattern in [
            "handleClick",
            "handleClick|openMenu",
            r"fn \w+_test",
            "(?i)error",
            "(abcdef)?ghijkl",
            "MAX_FILE_SIZE",
            r"handle.*\(\)",
            "ab",
            r"\w+",
        ] {
            let re = regex::Regex::new(pattern).unwrap();
            let q = plan_pattern(pattern, &ex).unwrap();
            let candidates = resolve_query(&q, docs.len() as u32, &lookup);
            for (i, d) in docs.iter().enumerate() {
                if re.is_match(std::str::from_utf8(d).unwrap()) {
                    assert!(
                        candidates.contains(&(i as u32)),
                        "pattern {pattern:?} matched doc {i} but candidates {candidates:?} miss it (plan {q:?})"
                    );
                }
            }
        }
    }

    fn class_of(pattern: &str) -> Class {
        match regex_syntax::Parser::new()
            .parse(pattern)
            .unwrap()
            .into_kind()
        {
            HirKind::Class(class) => class,
            other => panic!("{pattern} is not a class: {other:?}"),
        }
    }

    #[test]
    fn class_alternatives_expand_up_to_the_cap_and_refuse_past_it() {
        let unicode = class_alternatives(&class_of("[a-e]")).unwrap();
        assert_eq!(unicode.len(), 5);
        assert_eq!(unicode[0], b"a".to_vec());
        assert_eq!(class_alternatives(&class_of("[a-f]")), None);

        let bytes_class = class_of("(?-u)[a-c]");
        assert!(matches!(bytes_class, Class::Bytes(_)));
        assert_eq!(
            class_alternatives(&bytes_class),
            Some(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
        );
    }
}
