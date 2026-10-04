// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

/// A terminal the window fills to its caps: 88×24 at (6, 2), centre (50, 14).
const COLS: u16 = 100;
const ROWS: u16 = 28;

fn at(t: f32) -> Canvas {
    Intro::new().frame(COLS, ROWS, t).expect("100x28 fits")
}

fn screen(c: &Canvas) -> String {
    (0..c.h)
        .map(|y| c.row_text(y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn count(c: &Canvas, y: i32, ch: char) -> usize {
    c.row_text(y).chars().filter(|&x| x == ch).count()
}

// ── maths ───────────────────────────────────────────────────────────────────

#[test]
fn mix_interpolates_and_clamps() {
    let (black, white) = (Rgb(0, 0, 0), Rgb(255, 255, 255));
    assert_eq!(mix(black, white, 0.0), black);
    assert_eq!(mix(black, white, 1.0), white);
    assert_eq!(mix(black, white, 0.5), Rgb(128, 128, 128));
    assert_eq!(
        mix(Rgb(10, 200, 40), Rgb(30, 100, 40), 0.25),
        Rgb(15, 175, 40)
    );
    assert_eq!(mix(black, white, -1.0), black);
    assert_eq!(mix(black, white, 2.0), white);
}

#[test]
fn smooth_steps_between_its_edges() {
    assert_eq!(smooth(0.0, 1.0, -1.0), 0.0);
    assert_eq!(smooth(0.0, 1.0, 0.5), 0.5);
    assert_eq!(smooth(0.0, 1.0, 2.0), 1.0);
    assert_eq!(smooth(0.0, 2.0, 0.5), 0.156_25);
    assert_eq!(smooth(1.0, 3.0, 2.5), 0.843_75);
}

#[test]
fn easings_hit_their_anchors() {
    assert_eq!(ease_in_out(0.0), 0.0);
    assert_eq!(ease_in_out(0.25), 0.0625);
    assert_eq!(ease_in_out(0.5), 0.5);
    assert_eq!(ease_in_out(0.75), 0.9375);
    assert_eq!(ease_in_out(1.0), 1.0);
    assert_eq!(ease_in_out(3.0), 1.0);
    assert!(ease_out_back(0.0).abs() < 1e-6);
    assert!((ease_out_back(1.0) - 1.0).abs() < 1e-6);
    // it overshoots before settling: the landing bounce
    assert!((ease_out_back(0.5) - 1.087_697_5).abs() < 1e-5);
}

#[test]
fn hash3_is_fixed_per_input() {
    let raw = |a, b, c| (hash3(a, b, c) * 16_777_216.0) as u32;
    assert_eq!(raw(1, 2, 3), 11_971_386);
    assert_eq!(raw(2, 1, 3), 12_523_515);
    assert_eq!(raw(1, 2, 4), 4_874_462);
    assert_eq!(raw(7, -3, 11), 6_294_009);
    assert_eq!(raw(0, 0, 0), 0);
}

#[test]
fn sd_round_box_is_signed() {
    assert_eq!(sd_round_box(0.0, 0.0, 3.0, 3.0, 0.0), -3.0);
    assert_eq!(sd_round_box(-1.0, 0.0, 3.0, 2.0, 0.0), -2.0);
    assert_eq!(sd_round_box(3.0, 0.0, 3.0, 3.0, 0.0), 0.0);
    assert_eq!(sd_round_box(5.0, 0.0, 3.0, 3.0, 0.0), 2.0);
    assert_eq!(sd_round_box(0.0, -7.0, 3.0, 3.0, 0.0), 4.0);
    assert!((sd_round_box(5.0, 5.0, 3.0, 3.0, 0.0) - 8.0_f32.sqrt()).abs() < 1e-6);
    // a rounded corner sits further from the point than a square one
    assert!((sd_round_box(3.0, 3.0, 3.0, 3.0, 1.0) - (2.0_f32.sqrt() - 1.0)).abs() < 1e-6);
}

#[test]
fn ramp_glyphs_run_dense_to_sparse() {
    assert_eq!(ramp_at(0.0), '@');
    assert_eq!(ramp_at(0.5), '+');
    assert_eq!(ramp_at(1.0), '·');
    assert_eq!(ramp_at(-1.0), '@');
    assert_eq!(ramp_at(2.0), '·');
    assert_eq!(
        ramp_at(0.95),
        ':',
        "the factor (len - 1) keeps the last step"
    );
    assert_eq!(ramp_pick(0.0), '@');
    assert_eq!(ramp_pick(0.5), '+');
    assert_eq!(ramp_pick(0.999), '·');
    // the clamp keeps a boundary hash inside the ramp: `h * len` floors to
    // `len` at exactly 1.0, and `.min(len - 1)` must pull it back to the last
    // glyph instead of indexing past the end
    assert_eq!(ramp_pick(1.0), '·', "a boundary hash is clamped");
}

// ── canvas and layout ───────────────────────────────────────────────────────

#[test]
fn canvas_clips_and_keeps_backgrounds() {
    let mut c = Canvas::new(4, 2);
    assert_eq!(c.get(-1, 0), None);
    assert_eq!(c.get(4, 0), None);
    assert_eq!(c.get(0, 2), None);
    assert_eq!(c.get(0, -1), None);
    c.fill(0, 0, 4, 1, TERM);
    assert_eq!(c.text(1, 0, "abcd", GREEN, true), 5);
    assert_eq!(c.row_text(0), " abc");
    assert_eq!(
        c.get(1, 0),
        Some(Cell {
            ch: 'a',
            fg: GREEN,
            bg: Some(TERM),
            bold: true
        })
    );
    assert_eq!(c.get(0, 1), Some(BLANK));
    assert_eq!(c.row_text(1), "");
}

#[test]
fn the_window_centres_within_its_caps() {
    let win = Win::fit(100, 28).expect("fits");
    assert_eq!((win.x, win.y, win.w, win.h), (6, 2, 88, 24));
    assert_eq!(
        (win.cx0, win.cx1, win.log_top, win.log_rows),
        (9, 91, 6, 17)
    );
    assert_eq!((win.cx, win.cy, win.ax, win.ay), (50, 14, 50.0, 29.0));
    assert_eq!(win.status_row(), 24);
    assert!(win.inside(7, 3));
    assert!(win.inside(92, 24));
    assert!(!win.inside(6, 3));
    assert!(!win.inside(93, 3));
    assert!(!win.inside(7, 2));
    assert!(!win.inside(7, 25));
    assert_eq!(win.offset(50, 14), (0.5, 0.0));
    assert_eq!(win.offset(6, 2), (-43.5, -24.0));
    let small = Win::fit(64, 18).expect("the minimum fits");
    assert_eq!((small.x, small.y, small.w, small.h), (2, 1, 60, 16));
}

#[test]
fn fits_needs_64_by_18() {
    assert!(fits(MIN_COLS, MIN_ROWS));
    assert!(!fits(MIN_COLS - 1, MIN_ROWS));
    assert!(!fits(MIN_COLS, MIN_ROWS - 1));
    assert_eq!(Intro::new().frame(63, 18, 1.0), None);
}

#[test]
fn the_window_draws_its_corners_then_edges() {
    let c = at(1.0);
    // the frame of the act-1 window (6, 2, 88, 24), the corners on the four
    // exact cells and the edges filling the runs between them
    assert_eq!(c.get(6, 2).map(|cell| cell.ch), Some('╭'));
    assert_eq!(c.get(93, 2).map(|cell| cell.ch), Some('╮'));
    assert_eq!(c.get(6, 25).map(|cell| cell.ch), Some('╰'));
    assert_eq!(c.get(93, 25).map(|cell| cell.ch), Some('╯'));
    assert_eq!(c.get(30, 2).map(|cell| cell.ch), Some('─'));
    assert_eq!(c.get(30, 25).map(|cell| cell.ch), Some('─'));
    assert_eq!(c.get(6, 10).map(|cell| cell.ch), Some('│'));
    assert_eq!(c.get(93, 10).map(|cell| cell.ch), Some('│'));
}

#[test]
fn a_prompt_already_typed_shows_it_whole() {
    // the minimum window: y + 2 and y * 2 land on different rows, so a
    // flipped offset is visible
    let win = Win::fit(i32::from(MIN_COLS), i32::from(MIN_ROWS)).expect("the minimum fits");
    let mut c = Canvas::new(MIN_COLS, MIN_ROWS);
    c.fill(0, 0, i32::from(MIN_COLS), i32::from(MIN_ROWS), TERM);
    draw_prompt(&mut c, &win, 0.0, 0.0, 0.0);
    // typed zero means already there: the whole task is drawn, in green
    assert!(
        c.row_text(win.y + 2)
            .contains("› retry a leased push when the remote branch moved")
    );
    assert_eq!(c.get(win.cx0, win.y + 2).map(|cell| cell.fg), Some(GREEN));
}

#[test]
fn a_decode_line_shows_its_char_at_the_reveal_instant() {
    let win = Win::fit(i32::from(COLS), i32::from(ROWS)).expect("the window fits");
    let mut c = Canvas::new(COLS, ROWS);
    c.fill(0, 0, i32::from(COLS), i32::from(ROWS), TERM);
    let line = Line {
        at: 1.0,
        segs: vec![seg("x", INK, false)],
        typed: 0.0,
        decode: true,
    };
    let boundary = line.at + 0.05 + 0.22 * hash3(0, 0, 0);
    draw_line(&mut c, &win, win.log_top, &line, boundary, 0);
    assert_eq!(
        c.get(win.cx0, win.log_top).map(|cell| cell.ch),
        Some('x'),
        "at exactly its reveal instant a char is itself, not a ramp glyph"
    );
}

// ── act 1 ───────────────────────────────────────────────────────────────────

#[test]
fn highlight_paints_every_match() {
    let segs = highlight("released lease", "lease", NOISE, CORAL);
    let parts: Vec<(&str, Rgb)> = segs.iter().map(|s| (s.text.as_str(), s.fg)).collect();
    assert_eq!(
        parts,
        [
            ("re", NOISE),
            ("lease", CORAL),
            ("d ", NOISE),
            ("lease", CORAL),
            ("", NOISE)
        ]
    );
}

#[test]
fn read_steps_render_their_lines_plain() {
    let lines = scene1_lines();
    let segs = lines
        .iter()
        .find(|l| {
            l.segs
                .get(1)
                .is_some_and(|s| s.text.starts_with("//! `push`"))
        })
        .map(|l| &l.segs)
        .expect("the Read step's first output line");
    // a bare Read is not a grep: none of its output is painted as a hit
    assert!(
        segs.iter().all(|s| s.fg != CORAL),
        "Read output carries no coral hits"
    );
}

#[test]
fn every_step_ends_with_its_recorded_count() {
    let lines = scene1_lines();
    let summaries: Vec<String> = lines
        .iter()
        .filter(|l| l.segs.first().is_some_and(|s| s.text == "  ⎿  "))
        .map(|l| l.segs[1].text.clone())
        .collect();
    assert_eq!(
        summaries,
        [
            "1 line",
            "1 line",
            "122 lines",
            "20 lines",
            "Read 591 lines",
            "92 lines",
            "Read 100 lines",
            "23 lines",
            "60 lines",
            "37 lines",
            "31 lines"
        ]
    );
    // every line streams in before the next call and before the act ends
    assert!(lines.windows(2).all(|w| w[0].at <= w[1].at));
    assert!(lines.iter().all(|l| l.at < S1_END));
    let outputs: usize = STEPS.iter().map(|s| s.out.len()).sum();
    assert_eq!(lines.len(), STEPS.len() * 2 + outputs);
}

#[test]
fn the_prompt_types_itself() {
    let c = at(0.3);
    let title: String = c.row_text(2).chars().take(21).collect();
    assert_eq!(title, "      ╭─ without pixe");
    let prompt = c.row_text(4);
    assert!(
        prompt.contains("› retry a leased push when the r "),
        "{prompt}"
    );
    assert!(!prompt.contains("the re"), "{prompt}");
    assert_eq!(count(&c, 24, '…'), 0, "no status before the first call");
    assert!(at(0.6).row_text(24).contains("Searching…"));
    // a line shows from the very instant it is due
    assert!(at(0.6).row_text(6).contains("● Bash("));
    // the prompt mark is coral while the agent types, green in the pixel window
    assert_eq!(at(1.0).get(9, 4).map(|cell| cell.fg), Some(CORAL));
    assert_eq!(at(C_END + 1.0).get(9, 4).map(|cell| cell.fg), Some(GREEN));
    assert!(!at(0.59).row_text(24).contains("Searching…"));
}

#[test]
fn the_struggle_shows_the_recorded_calls() {
    let c = at(1.0);
    let s = screen(&c);
    assert!(
        s.contains(r#"│  ● Bash(grep -rln "lease" --include=*.sh"#),
        "{s}"
    );
    assert!(s.contains("│    ⎿  1 line"), "{s}");
    let status = c.row_text(24);
    assert!(
        status.contains("✳ Searching…"),
        "the spinner turns at 12 frames a second: {status}"
    );
    assert!(at(0.6).row_text(24).contains("✶ Searching…"));
    assert!(status.contains("context ▕█░░░░░░░░░░░░░░░▏"), "{status}");
    // a command wider than the window ends on an ellipsis at the margin
    assert_eq!(c.get(90, 6).map(|cell| cell.ch), Some('…'));
}

#[test]
fn the_read_step_scrolls_the_file() {
    let c = at(2.3);
    let status = c.row_text(24);
    assert!(status.contains("Reading push.rs…"), "{status}");
    // 735 of 1080 recorded lines read: 11 of 16 gauge cells
    assert_eq!(count(&c, 24, '█'), 11);
    // the gauge warms from coral-dim to coral, the rest stays unlit
    assert_eq!(c.get(74, 24).map(|cell| cell.fg), Some(CORAL_DIM));
    assert_eq!(
        c.get(82, 24).map(|cell| cell.fg),
        Some(mix(CORAL_DIM, CORAL, 0.5))
    );
    assert_eq!(
        c.get(85, 24).map(|cell| (cell.ch, cell.fg)),
        Some(('░', LINE))
    );
    // every line read lights the whole gauge by the last call
    assert_eq!(count(&at(3.8), 24, '█'), 16);
    let s = screen(&c);
    // the header scrolled off: the newest line sits on the last log row
    assert!(!s.contains("● Read(crates/pixel-ops/src/push.rs)"), "{s}");
    let last = c.row_text(22);
    assert!(
        last.starts_with(&format!("      │       {}", data::PUSH_RS[36])),
        "{last}"
    );
    // a line wider than the window keeps its head and ends on an ellipsis
    assert!(
        s.contains("is stripped from the source side — pixel expres…"),
        "{s}"
    );
}

// ── act 2 ───────────────────────────────────────────────────────────────────

#[test]
fn collapse_starts_from_the_struggle() {
    let before = at(S1_END - 1e-3);
    let after = at(S1_END);
    assert_eq!(after.row_text(2), before.row_text(2));
    assert_eq!(after.row_text(24), before.row_text(24));
    let win = Win::fit(100, 28).expect("fits");
    for y in win.y + 1..win.y + win.h - 1 {
        for x in win.x + 1..win.x + win.w - 1 {
            let (dx, dy) = win.offset(x, y);
            if dx.hypot(dy) > 2.0 {
                // a blank's colour is invisible: compare what is drawn
                let (a, b) = (after.get(x, y), before.get(x, y));
                let drawn = |cell: Option<Cell>| cell.filter(|cell| cell.ch != ' ');
                assert_eq!(drawn(a), drawn(b), "({x}, {y})");
                assert_eq!(a.map(|cell| cell.ch), b.map(|cell| cell.ch), "({x}, {y})");
            }
        }
    }
    // the centre already glows: the core the noise falls into
    let core = after.get(50, 14).expect("on screen");
    assert_eq!(
        (core.fg, core.bold),
        (mix(GREEN_HI, GREEN_DIM, 1.0 / 3.0), true)
    );
}

#[test]
fn collapse_absorbs_every_character() {
    let c = at(A_END - 1e-3);
    let win = Win::fit(100, 28).expect("fits");
    for y in win.y + 1..win.y + win.h - 1 {
        for x in win.x + 1..win.x + win.w - 1 {
            let (dx, dy) = win.offset(x, y);
            if dx.hypot(dy) > 4.5 {
                assert_eq!(
                    c.get(x, y).map(|cell| cell.ch),
                    Some(' '),
                    "({x}, {y}): {}",
                    screen(&c)
                );
            }
        }
    }
    // the title has faded into the ground
    assert_eq!(
        c.get(9, 2).map(|cell| (cell.ch, cell.fg)),
        Some(('w', TERM))
    );
}

#[test]
fn the_pixel_pulses_at_the_centre() {
    let c = at(B_END - 1e-3);
    for y in 13..=15 {
        for x in 47..53 {
            assert_eq!(
                c.get(x, y).map(|cell| (cell.ch, cell.fg)),
                Some(('█', GREEN)),
                "({x}, {y})"
            );
        }
        assert_ne!(c.get(46, y).map(|cell| cell.ch), Some('█'));
        assert_ne!(c.get(53, y).map(|cell| cell.ch), Some('█'));
    }
    assert_ne!(c.get(50, 12).map(|cell| cell.ch), Some('█'));
    assert_ne!(c.get(50, 16).map(|cell| cell.ch), Some('█'));
    assert_eq!(pixel_cells(&Win::fit(100, 28).expect("fits")).count(), 18);
    // at its birth the pixel flashes towards white
    assert_eq!(
        at(A_END).get(50, 14).map(|cell| cell.fg),
        Some(mix(GREEN, INK, 0.85))
    );
}

#[test]
fn the_acts_follow_each_other() {
    assert_eq!(act(0.0), Act::Struggle);
    assert_eq!(act(S1_END - 1e-3), Act::Struggle);
    assert_eq!(act(S1_END), Act::Collapse);
    assert_eq!(act(A_END), Act::Pulse);
    assert_eq!(act(B_END), Act::Bloom);
    assert_eq!(act(C_END), Act::Answer);
    assert_eq!(act(S3_END), Act::Wordmark);
    assert_eq!(act(END), Act::Wordmark);
    // the bloom starts from the pixel, not from a finished pixel frame
    assert_ne!(at(B_END).get(50, 14).map(|cell| cell.ch), Some('█'));
}

#[test]
fn bloom_ends_on_the_pixel_window() {
    assert_eq!(at(C_END - 1e-4), at(C_END));
    let mid = screen(&at((B_END + C_END) / 2.0));
    assert!(mid.contains('@'), "the rim is drawn in the ramp: {mid}");
    assert!(mid.contains('░'), "a halo rings the rim: {mid}");
}

// ── act 3 ───────────────────────────────────────────────────────────────────

#[test]
fn the_pixel_window_answers_with_three_calls() {
    let c = at(S3_END - 1e-3);
    let s = screen(&c);
    for expected in [
        "╭─ ■ with pixel ─",
        "› retry a leased push when the remote branch moved",
        r#"● pixel scope-task "retry a leased push when the remote branch moved""#,
        "⎿  P0  crates/pixel-ops/src/push.rs        PushOptions · build_push_args",
        "    P1  crates/pixel/src/main.rs            CommitAndPush · decide_remote",
        "● pixel who-calls push_with_state --role callers",
        "⎿  [exact]     push                 crates/pixel-ops/src/push.rs:129",
        "crates/pixel-ops/tests/all/crash_matrix.rs:…",
        "● pixel pack-context crates/pixel-ops/src/push.rs#push_with_state#function",
        "⎿  push.rs:132-229  pub fn push_with_state(root: &Path, opts: &PushOptions, …)",
        "✓ context ready  ranked files · their callers · the function itself",
        "■ pixel indexed · local · deterministic",
    ] {
        assert!(s.contains(expected), "missing {expected:?} in\n{s}");
    }
    assert!(!s.contains("without pixel"));
    // the first rank is green, the next tier soft
    assert_eq!(
        c.get(14, 7).map(|cell| (cell.ch, cell.fg)),
        Some(('P', GREEN))
    );
    assert_eq!(
        c.get(14, 11).map(|cell| (cell.ch, cell.fg)),
        Some(('P', SOFT))
    );
    // the window is closed on all four corners
    let bottom = c.row_text(25);
    assert_eq!(bottom, format!("      ╰{}╯", "─".repeat(86)));
    assert!(c.row_text(2).ends_with('╮'));
    assert!(c.row_text(3).starts_with("      │") && c.row_text(3).ends_with('│'));
}

#[test]
fn answers_resolve_out_of_the_ramp() {
    let early = at(C_END + 0.621).row_text(7);
    let body = early.trim_matches(['│', ' ']);
    assert!(!body.is_empty(), "the ramp stands in: {early}");
    assert!(
        !body.chars().any(char::is_alphanumeric),
        "nothing resolved yet: {early}"
    );
    // the ramp flickers from one tick to the next while it waits
    assert_ne!(at(C_END + 0.655).row_text(7), early);
    let settled = at(C_END + 0.9).row_text(7);
    assert!(
        settled.contains("crates/pixel-ops/src/push.rs"),
        "{settled}"
    );
    // a command types itself out: halfway through, half of it is there
    let half = at(C_END + 0.15 + 0.175).row_text(6);
    assert!(
        half.contains("● pixel scope-task \"retry a lease"),
        "{half}"
    );
    assert!(!half.contains("branch moved"), "{half}");
}

// ── act 4 ───────────────────────────────────────────────────────────────────

#[test]
fn the_wordmark_has_51_pixels() {
    let (pixels, width) = wordmark();
    assert_eq!((pixels.len(), width), (51, 25));
    assert_eq!(pixels[..4], [(0, 0), (1, 0), (2, 0), (3, 0)]);
    assert!(pixels.contains(&(24, 4)), "the L's foot closes the word");
}

#[test]
fn the_wordmark_settles_whole() {
    let c = at(END);
    let s = screen(&c);
    let blocks: usize = (0..c.h).map(|y| count(&c, y, '█')).sum();
    assert_eq!(blocks, 102, "51 pixels, two columns each:\n{s}");
    let top = c.row_text(10);
    let expected = format!(
        "      │{}████████    ██████  ██      ██  ████████  ██",
        " ".repeat(18)
    );
    assert!(top.starts_with(&expected), "{top}");
    assert!(c.row_text(17).contains(TAGLINE), "{s}");
    assert!(c.row_text(19).contains(SETTING_UP), "{s}");
    assert!(
        !s.contains("pixel scope-task"),
        "the pixel window dissolved:\n{s}"
    );
    assert!(!at(S3_END + 2.1).row_text(19).contains(SETTING_UP));
}

#[test]
fn default_is_new() {
    assert_eq!(
        Intro::default().frame(COLS, ROWS, 2.0),
        Intro::new().frame(COLS, ROWS, 2.0)
    );
}

#[test]
fn particles_follow_the_terminal_size() {
    let t = A_END - 0.6;
    let mut intro = Intro::new();
    let _ = intro.frame(COLS, ROWS, t);
    let resized = intro.frame(120, 30, t);
    assert_eq!(resized, Intro::new().frame(120, 30, t));
}

// ── terminal bytes ──────────────────────────────────────────────────────────

const SYNC_OPEN: &str = "\x1b[?2026h";
const SYNC_CLOSE: &str = "\x1b[0m\x1b[?2026l";

#[test]
fn an_unchanged_frame_writes_nothing_but_the_sync_bracket() {
    let c = at(1.0);
    assert_eq!(diff(&c, Some(&c), true), format!("{SYNC_OPEN}{SYNC_CLOSE}"));
}

#[test]
fn a_first_frame_paints_every_cell() {
    let c = Canvas::new(2, 1);
    assert_eq!(
        diff(&c, None, true),
        format!("{SYNC_OPEN}\x1b[1;1H\x1b[22m\x1b[38;2;236;247;239m\x1b[49m  {SYNC_CLOSE}")
    );
    // a previous frame of another size counts as none
    assert_eq!(
        diff(&c, Some(&Canvas::new(3, 1)), true),
        diff(&c, None, true)
    );
}

#[test]
fn changed_cells_move_the_cursor_once_per_run() {
    let prev = Canvas::new(6, 2);
    let mut cur = prev.clone();
    cur.put(3, 1, 'x', GREEN, true);
    cur.put(4, 1, 'y', GREEN, true);
    cur.set(
        0,
        0,
        Cell {
            ch: 'z',
            fg: GREEN,
            bg: Some(TERM),
            bold: false,
        },
    );
    assert_eq!(
        diff(&cur, Some(&prev), true),
        format!(
            "{SYNC_OPEN}\x1b[1;1H\x1b[22m\x1b[38;2;34;197;94m\x1b[48;2;7;22;15mz\
             \x1b[2;4H\x1b[1m\x1b[49mxy{SYNC_CLOSE}"
        )
    );
}

#[test]
fn without_truecolor_the_256_cube_stands_in() {
    let prev = Canvas::new(1, 1);
    let mut cur = prev.clone();
    cur.set(
        0,
        0,
        Cell {
            ch: 'z',
            fg: GREEN,
            bg: Some(TERM),
            bold: false,
        },
    );
    assert_eq!(
        diff(&cur, Some(&prev), false),
        format!("{SYNC_OPEN}\x1b[1;1H\x1b[22m\x1b[38;5;41m\x1b[48;5;16mz{SYNC_CLOSE}")
    );
}

#[test]
fn to_256_quantises_each_channel() {
    assert_eq!(to_256(Rgb(0, 0, 0)), 16);
    assert_eq!(to_256(Rgb(47, 48, 114)), 23);
    assert_eq!(to_256(Rgb(115, 255, 0)), 118);
    assert_eq!(to_256(Rgb(255, 255, 255)), 231);
    assert_eq!(to_256(GREEN), 41);
}
