// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

fn limits(chars: usize, bytes: usize) -> Limits {
    Limits { chars, bytes }
}

/// Lines of `width` characters (newline included), `count` of them.
fn lines(width: usize, count: usize) -> String {
    let line = format!("{}\n", "x".repeat(width - 1));
    line.repeat(count)
}

fn part(index: u8, count: u8) -> BriefPart {
    BriefPart { index, count }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pixel-brief-parts-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn lengths(pieces: &[&str]) -> Vec<usize> {
    pieces.iter().map(|piece| piece.chars().count()).collect()
}

#[test]
fn a_piece_holds_exactly_the_limit_and_the_next_character_starts_another() {
    assert_eq!(PART_CHARS, 9_800);
    assert_eq!(split("", Limits::CHARS), Vec::<&str>::new());
    let exact = "a".repeat(9_800);
    assert_eq!(split(&exact, Limits::CHARS), vec![exact.as_str()]);
    let over = "a".repeat(9_801);
    assert_eq!(lengths(&split(&over, Limits::CHARS)), vec![9_800, 1]);
    let under = "a".repeat(9_799);
    assert_eq!(lengths(&split(&under, Limits::CHARS)), vec![9_799]);
    let twice = "a".repeat(19_600);
    assert_eq!(lengths(&split(&twice, Limits::CHARS)), vec![9_800, 9_800]);
    let thrice = "a".repeat(19_601);
    assert_eq!(
        lengths(&split(&thrice, Limits::CHARS)),
        vec![9_800, 9_800, 1]
    );
}

#[test]
fn whole_lines_fill_a_piece_and_the_line_that_does_not_fit_opens_the_next() {
    // 98 lines of 100 characters are exactly one piece.
    let full = lines(100, 98);
    assert_eq!(full.chars().count(), 9_800);
    assert_eq!(split(&full, Limits::CHARS), vec![full.as_str()]);
    // The 99th line opens the second piece whole.
    let more = lines(100, 99);
    let pieces = split(&more, Limits::CHARS);
    assert_eq!(lengths(&pieces), vec![9_800, 100]);
    assert_eq!(pieces[1], lines(100, 1));
    // A line that would cross the boundary moves to the next piece: 9,750
    // characters of lines, then one of 100, then a short one.
    let text = format!("{}{}{}", lines(150, 65), lines(100, 1), "tail\n");
    assert_eq!(text.chars().count(), 9_750 + 100 + 5);
    let pieces = split(&text, Limits::CHARS);
    assert_eq!(lengths(&pieces), vec![9_750, 105]);
    assert!(pieces[1].starts_with(&lines(100, 1)));
    assert!(pieces.iter().all(|piece| piece.ends_with('\n')));
}

#[test]
fn a_line_longer_than_a_piece_is_cut_inside_it_and_the_rest_continues() {
    let text = format!("head\n{}\ntail\n", "y".repeat(25_000));
    let pieces = split(&text, Limits::CHARS);
    assert_eq!(pieces.concat(), text);
    assert_eq!(
        pieces[0], "head\n",
        "the lines before it stay a piece of their own"
    );
    assert!(pieces.iter().all(|piece| piece.chars().count() <= 9_800));
    assert_eq!(lengths(&pieces), vec![5, 9_800, 9_800, 5_406]);
    assert_eq!(pieces[3], format!("{}\ntail\n", "y".repeat(5_400)));
}

#[test]
fn the_character_limit_counts_characters_not_bytes() {
    // 9,800 two-byte characters are 19,600 bytes and still one piece for a
    // host that counts characters; one more character starts another.
    let text = "é".repeat(9_800);
    assert_eq!(split(&text, Limits::CHARS).len(), 1);
    assert_eq!(
        lengths(&split(&"é".repeat(9_801), Limits::CHARS)),
        vec![9_800, 1]
    );
    let emoji = "🦀".repeat(9_801);
    let pieces = split(&emoji, Limits::CHARS);
    assert_eq!(lengths(&pieces), vec![9_800, 1]);
    assert_eq!(pieces.concat(), emoji);
}

#[test]
fn the_byte_limit_cuts_on_character_boundaries_for_a_host_that_counts_bytes() {
    // Two-byte characters: 4,900 fit 9,800 bytes.
    let text = "é".repeat(9_800);
    let pieces = split(&text, Limits::BYTES);
    assert_eq!(lengths(&pieces), vec![4_900, 4_900]);
    assert!(pieces.iter().all(|piece| piece.len() <= 9_800));
    // Four-byte characters: 2,450 fit.
    let crabs = "🦀".repeat(2_451);
    let pieces = split(&crabs, Limits::BYTES);
    assert_eq!(lengths(&pieces), vec![2_450, 1]);
    // ASCII is the same under both.
    let ascii = "a".repeat(9_801);
    assert_eq!(split(&ascii, Limits::BYTES), split(&ascii, Limits::CHARS));
    // A multibyte character that would cross the byte boundary moves whole.
    let edge = format!("{}é", "a".repeat(9_799));
    let pieces = split(&edge, Limits::BYTES);
    assert_eq!(pieces, vec!["a".repeat(9_799).as_str(), "é"]);
}

#[test]
fn a_limit_smaller_than_one_character_still_makes_progress() {
    let pieces = split("🦀🦀", limits(0, 0));
    assert_eq!(pieces, vec!["🦀", "🦀"]);
    assert_eq!(
        split("ab\ncd", limits(1, 1)),
        vec!["a", "b", "\n", "c", "d"]
    );
}

#[test]
fn pieces_always_concatenate_to_the_text_and_never_exceed_the_limits() {
    // A deterministic mix of line lengths, multibyte characters and blank lines.
    let mut seed = 0x2545_f491_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut text = String::new();
    for _ in 0..400 {
        let width = usize::try_from(next() % 900).unwrap();
        for _ in 0..width {
            text.push(match next() % 7 {
                0 => 'é',
                1 => '字',
                2 => '🦀',
                _ => 'a',
            });
        }
        text.push('\n');
    }
    for limit in [
        Limits::CHARS,
        Limits::BYTES,
        limits(300, usize::MAX),
        limits(64, 100),
    ] {
        let pieces = split(&text, limit);
        assert_eq!(pieces.concat(), text, "{limit:?}");
        assert!(
            pieces
                .iter()
                .all(|piece| piece.chars().count() <= limit.chars && piece.len() <= limit.bytes),
            "{limit:?}"
        );
        assert!(pieces.iter().all(|piece| !piece.is_empty()), "{limit:?}");
        assert_eq!(pieces, split(&text, limit), "deterministic");
    }
}

#[test]
fn a_slice_is_the_indexed_piece_and_empty_past_the_end() {
    let text = lines(100, 99);
    assert_eq!(slice(&text, part(1, 3), Limits::CHARS), lines(100, 98));
    assert_eq!(slice(&text, part(2, 3), Limits::CHARS), lines(100, 1));
    assert_eq!(slice(&text, part(3, 3), Limits::CHARS), "");
    assert_eq!(slice("", part(1, 3), Limits::CHARS), "");
    // Parts together are the brief.
    let big = lines(100, 250);
    let joined: String = (1..=3)
        .map(|k| slice(&big, part(k, 3), Limits::CHARS))
        .collect();
    assert_eq!(joined, big, "250 lines are exactly three pieces");
    // A brief longer than the parts is cut after the last one, whole lines.
    let huge = lines(100, 400);
    let joined: String = (1..=3)
        .map(|k| slice(&huge, part(k, 3), Limits::CHARS))
        .collect();
    assert_eq!(joined, lines(100, 98 * 3));
}

#[test]
fn a_part_is_k_over_n_inside_the_bounds() {
    assert_eq!("1/3".parse::<BriefPart>(), Ok(part(1, 3)));
    assert_eq!("3/3".parse::<BriefPart>(), Ok(part(3, 3)));
    assert_eq!("1/1".parse::<BriefPart>(), Ok(part(1, 1)));
    let most = pixel_install::config::BRIEF_PARTS_MAX;
    assert_eq!(
        format!("{most}/{most}").parse::<BriefPart>(),
        Ok(part(most, most))
    );
    for bad in [
        "0/3", "4/3", "1/9", "2", "/3", "1/", "a/b", "1/3/3", "", "-1/3", "1/0", " 1/3",
    ] {
        let error = bad.parse::<BriefPart>().unwrap_err();
        assert!(error.contains("K/N"), "{bad}: {error}");
    }
}

#[test]
fn the_stored_brief_is_keyed_by_session_and_prompt_and_names_neither() {
    let root = Path::new("/repo");
    let base = cache_path(root, Some("session-1"), "what does X do?");
    assert_eq!(base, cache_path(root, Some("session-1"), "what does X do?"));
    assert_ne!(base, cache_path(root, Some("session-2"), "what does X do?"));
    assert_ne!(base, cache_path(root, Some("session-1"), "what does Y do?"));
    assert_ne!(
        base,
        cache_path(Path::new("/other"), Some("session-1"), "what does X do?")
    );
    assert_eq!(cache_path(root, None, "p"), cache_path(root, Some(""), "p"));
    let name = base.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        base.parent().unwrap(),
        Path::new("/repo/.pixel/brief-parts")
    );
    assert!(
        !name.contains("session") && !name.contains("what"),
        "{name}"
    );
    let (session, prompt) = name.strip_suffix(".txt").unwrap().split_once('-').unwrap();
    assert_eq!((session.len(), prompt.len()), (16, 64));
    assert!(name.bytes().all(|b| b.is_ascii_hexdigit()
        || b == b'-'
        || b == b'.'
        || b == b't'
        || b == b'x'));
    // The hash is the SHA-256 of the prompt.
    assert_eq!(
        hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_stored_brief_is_owner_only_and_read_back_whole() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("store");
    let path = cache_path(&root, Some("s"), "p");
    let text = format!("{}é\n", lines(100, 200));
    store(&path, &text, SystemTime::now()).unwrap();
    assert_eq!(
        wait_for(&path, Duration::ZERO, POLL).as_deref(),
        Some(text.as_str())
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A later prompt's brief replaces it.
    store(&path, "second", SystemTime::now()).unwrap();
    assert_eq!(
        wait_for(&path, Duration::ZERO, POLL).as_deref(),
        Some("second")
    );
    let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(leftovers.len(), 1, "no temporary file stays: {leftovers:?}");
}

#[test]
fn an_empty_brief_is_stored_so_the_other_parts_do_not_wait() {
    let root = scratch("empty");
    let path = cache_path(&root, Some("s"), "p");
    store(&path, "", SystemTime::now()).unwrap();
    mark_done(&path);
    let started = Instant::now();
    assert_eq!(
        wait_for(&path, Duration::from_secs(5), POLL).as_deref(),
        Some("")
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn waiting_for_a_brief_that_never_comes_ends_at_the_bound_with_nothing() {
    let root = scratch("never");
    let path = cache_path(&root, Some("s"), "p");
    let started = Instant::now();
    assert_eq!(
        wait_for(&path, Duration::from_millis(120), Duration::from_millis(5)),
        None
    );
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(120), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
}

#[test]
fn a_part_that_waits_gets_the_brief_the_moment_part_one_stores_it() {
    let root = scratch("late");
    let path = cache_path(&root, Some("s"), "p");
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            store(&path, "late brief", SystemTime::now()).unwrap();
            mark_done(&path);
        })
    };
    let started = Instant::now();
    let got = wait_for(&path, Duration::from_secs(10), Duration::from_millis(5));
    assert_eq!(got.as_deref(), Some("late brief"));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(100),
        "it waited: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "and stopped when it came: {waited:?}"
    );
    writer.join().unwrap();
}

#[test]
fn a_stored_brief_is_read_up_to_its_cap_and_no_further() {
    let root = scratch("cap");
    let path = cache_path(&root, Some("s"), "p");
    let size = usize::try_from(MAX_STORED_BYTES).unwrap();
    store(&path, &"a".repeat(size + 5_000), SystemTime::now()).unwrap();
    let got = wait_for(&path, Duration::ZERO, POLL).unwrap();
    assert_eq!(got.len(), size);
}

#[test]
fn a_linked_directory_or_file_is_never_followed() {
    use std::os::unix::fs::symlink;
    let root = scratch("links");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    // `.pixel/brief-parts` is a link to a directory elsewhere.
    fs::create_dir_all(root.join(".pixel")).unwrap();
    symlink(&outside, root.join(".pixel/brief-parts")).unwrap();
    let path = cache_path(&root, Some("s"), "p");
    assert!(store(&path, "secret", SystemTime::now()).is_err());
    assert_eq!(
        fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing written through it"
    );
    fs::write(outside.join(path.file_name().unwrap()), "planted").unwrap();
    assert_eq!(
        wait_for(&path, Duration::ZERO, POLL),
        None,
        "and nothing read through it"
    );
    forget(&path);
    assert!(
        outside.join(path.file_name().unwrap()).exists(),
        "nor removed"
    );

    // `.pixel` itself is a link.
    let root = scratch("links-pixel");
    symlink(&outside, root.join(".pixel")).unwrap();
    assert!(
        store(
            &cache_path(&root, Some("s"), "p"),
            "secret",
            SystemTime::now()
        )
        .is_err()
    );

    // The file is a link to another file: stored over it, not through it.
    let root = scratch("links-file");
    let path = cache_path(&root, Some("s"), "p");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let target = root.join("target.txt");
    fs::write(&target, "untouched").unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        wait_for(&path, Duration::ZERO, POLL),
        None,
        "a link is not a brief"
    );
    store(&path, "mine", SystemTime::now()).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    assert_eq!(
        wait_for(&path, Duration::ZERO, POLL).as_deref(),
        Some("mine")
    );
}

#[test]
fn forgetting_removes_the_stored_brief_and_only_that_one() {
    let root = scratch("forget");
    let one = cache_path(&root, Some("s"), "one");
    let two = cache_path(&root, Some("s"), "two");
    store(&one, "1", SystemTime::now()).unwrap();
    store(&two, "2", SystemTime::now()).unwrap();
    forget(&one);
    assert_eq!(wait_for(&one, Duration::ZERO, POLL), None);
    assert_eq!(wait_for(&two, Duration::ZERO, POLL).as_deref(), Some("2"));
    forget(&one);
}

#[test]
fn pruning_deletes_what_is_older_than_the_age_and_keeps_the_rest() {
    let dir = scratch("prune");
    let now = SystemTime::now();
    let age = Duration::from_secs(3_600);
    let write = |name: &str, written: SystemTime| {
        let path = dir.join(name);
        fs::write(&path, name).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(written)
            .unwrap();
    };
    write("old.txt", now - age - Duration::from_secs(1));
    write("older.txt", now - age * 5);
    write("edge.txt", now - age);
    write("fresh.txt", now - Duration::from_secs(5));
    write("future.txt", now + Duration::from_secs(600));
    fs::create_dir(dir.join("subdir")).unwrap();
    assert_eq!(prune(&dir, now, age), 2);
    let mut left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, vec!["edge.txt", "fresh.txt", "future.txt", "subdir"]);
    assert_eq!(prune(&dir, now, age), 0);
    assert_eq!(prune(&dir.join("absent"), now, age), 0);
}

#[test]
fn storing_prunes_a_brief_older_than_an_hour() {
    let root = scratch("store-prune");
    let old = cache_path(&root, Some("s"), "old");
    let new = cache_path(&root, Some("s"), "new");
    let now = SystemTime::now();
    store(&old, "old", now).unwrap();
    store(&new, "new", now + MAX_AGE + Duration::from_secs(60)).unwrap();
    assert_eq!(
        wait_for(&old, Duration::ZERO, POLL),
        None,
        "an hour and a minute later"
    );
    assert_eq!(wait_for(&new, Duration::ZERO, POLL).as_deref(), Some("new"));
    assert_eq!(MAX_AGE, Duration::from_secs(3_600));
}

#[test]
#[cfg(debug_assertions)]
fn padding_makes_exactly_the_parts_asked_each_ending_in_its_marker() {
    assert_eq!(FILLER.len(), 100);
    assert!(FILLER.is_ascii() && FILLER.ends_with('\n') && FILLER.matches('\n').count() == 1);
    for limit in [
        Limits::CHARS,
        Limits::BYTES,
        limits(500, 500),
        limits(1_000, usize::MAX),
    ] {
        for count in 1..=4_u8 {
            for brief in ["", "[PIXEL:BRIEF]\nline é 🦀\n", "no newline at the end"] {
                let padded = pad_to_parts(brief, count, limit);
                let pieces = split(&padded, limit);
                assert_eq!(
                    pieces.len(),
                    usize::from(count),
                    "{limit:?} {count} {brief:?}"
                );
                for (k, piece) in (1..=count).zip(&pieces) {
                    assert!(
                        piece.ends_with(&marker(k, count)),
                        "{k} of {count}: {piece:?}"
                    );
                    assert!(piece.chars().count() <= limit.chars && piece.len() <= limit.bytes);
                }
                assert!(padded.starts_with(brief.trim_end_matches('\n')));
            }
        }
    }
    assert_eq!(marker(2, 3), "PIXEL-PART-MARKER-2-OF-3\n");
}

#[test]
fn a_part_holds_back_until_part_one_is_done_and_then_goes_at_once() {
    let root = scratch("order");
    let path = cache_path(&root, Some("s"), "p");
    store(&path, "the brief", SystemTime::now()).unwrap();
    let finisher = {
        let path = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            mark_done(&path);
        })
    };
    let started = Instant::now();
    let got = wait_for(&path, Duration::from_secs(20), Duration::from_millis(5));
    let waited = started.elapsed();
    assert_eq!(got.as_deref(), Some("the brief"));
    assert!(
        waited >= Duration::from_millis(200),
        "held back for part 1: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "and went when it was done: {waited:?}"
    );
    finisher.join().unwrap();
}

#[test]
fn a_part_whose_first_never_finishes_still_prints_at_the_bound() {
    let root = scratch("unfinished");
    let path = cache_path(&root, Some("s"), "p");
    store(&path, "the brief", SystemTime::now()).unwrap();
    let started = Instant::now();
    let got = wait_for(&path, Duration::from_millis(200), Duration::from_millis(5));
    assert_eq!(got.as_deref(), Some("the brief"));
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn the_done_marker_is_owner_only_and_belongs_to_one_stored_brief() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("done");
    let path = cache_path(&root, Some("s"), "p");
    store(&path, "one", SystemTime::now()).unwrap();
    mark_done(&path);
    let done = path.with_extension("done");
    assert_eq!(
        fs::metadata(&done).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(done_path(&path), done);
    assert_eq!(fs::metadata(&done).unwrap().len(), 0);
    // A new brief for the same prompt starts without the last one's marker.
    store(&path, "two", SystemTime::now()).unwrap();
    assert!(!done.exists());
    mark_done(&path);
    forget(&path);
    assert!(!path.exists() && !done.exists(), "forgetting takes both");
}

#[test]
fn the_done_marker_is_never_written_through_a_link() {
    use std::os::unix::fs::symlink;
    let root = scratch("done-links");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(root.join(".pixel")).unwrap();
    symlink(&outside, root.join(".pixel/brief-parts")).unwrap();
    mark_done(&cache_path(&root, Some("s"), "p"));
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}
