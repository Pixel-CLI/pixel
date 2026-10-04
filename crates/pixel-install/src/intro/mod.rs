//! The intro an interactive `pixel install` plays before its banner, in
//! four acts on one clock ([`END`] seconds):
//!
//! 1. **without pixel** — an agent greps its way through a repository,
//!    faster and faster, until the text breaks up;
//! 2. **collapse** — the noise spirals into the centre and condenses into
//!    one green pixel, the logo, which pulses rings out;
//! 3. **bloom** — the pixel grows into a fresh window where the same task
//!    takes three pixel calls;
//! 4. **wordmark** — PIXEL drops in, pixel by pixel, and shimmers.
//!
//! Act 1 replays the commands and line counts of the recorded vanilla-6 run
//! of the demo task (`docs/motion/src/demo/runs.json`) over real grep output
//! ([`data`]); act 3 shows what this repository's index answers for that
//! task. Neither carries a total or a timer: the recorded medians support no
//! speed or token claim on this task (`docs/motion/README.md`).
//!
//! Pure: [`Intro::frame`] draws the picture at a time and [`diff`] turns two
//! pictures into terminal bytes. The caller owns the tty.

mod data;

use std::fmt::Write as _;

/// One 24-bit colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

const fn hex(v: u32) -> Rgb {
    let [_, r, g, b] = v.to_be_bytes();
    Rgb(r, g, b)
}

// The website's identity (`docs/motion/src/theme.ts`): forest green ground,
// coral for what an agent wastes, green for what pixel hands back.
const TERM: Rgb = hex(0x07_16_0F);
const LINE: Rgb = hex(0x1F_45_35);
const INK: Rgb = hex(0xEC_F7_EF);
const SOFT: Rgb = hex(0x93_B3_A0);
const FAINT: Rgb = hex(0x3D_6B_54);
const NOISE: Rgb = hex(0x55_80_6A);
const GREEN: Rgb = hex(0x22_C5_5E);
const GREEN_HI: Rgb = hex(0x4A_DE_80);
const GREEN_DIM: Rgb = hex(0x2F_7A_4F);
const CORAL: Rgb = hex(0xF0_77_5A);
const CORAL_DIM: Rgb = hex(0x6B_3A_2E);
const CORAL_INK: Rgb = hex(0xFF_B4_A1);

/// Dense to sparse: the shading every ghostly edge is drawn with.
const RAMP: [char; 11] = ['@', '$', '%', '#', '*', '+', '=', '~', '-', ':', '·'];
const SPIN: [char; 10] = ['·', '✢', '✳', '✶', '✻', '✽', '✻', '✶', '✳', '✢'];

// The timeline, in seconds.
/// Act 1 ends: the agent's window breaks up.
const S1_END: f32 = 4.3;
/// The noise has spiralled into the centre.
const A_END: f32 = 5.45;
/// The pixel has pulsed.
const B_END: f32 = 6.05;
/// The pixel has bloomed into the new window.
const C_END: f32 = 6.85;
/// The pixel window has answered.
const S3_END: f32 = 10.55;
/// The last frame: the wordmark has settled.
pub const END: f32 = 13.55;

/// The smallest terminal the intro draws in: a 60×16 window plus margins.
pub const MIN_COLS: u16 = 64;
pub const MIN_ROWS: u16 = 18;

/// Switch to the alternate screen and hide the cursor; [`LEAVE`] undoes it.
pub const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J";
/// Reset the colours, show the cursor and return to the main screen.
pub const LEAVE: &str = "\x1b[0m\x1b[?25h\x1b[?1049l";
/// Wipe the screen after a resize, so no cell of the old layout survives.
pub const CLEAR: &str = "\x1b[0m\x1b[2J";

// ── maths ───────────────────────────────────────────────────────────────────

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Hermite step from 0 at `a` to 1 at `b`.
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp01((x - a) / (b - a));
    t * t * (3.0 - 2.0 * t)
}

fn ease_in_out(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Overshoots past 1 before settling: a pixel landing with a bounce.
fn ease_out_back(t: f32) -> f32 {
    let t = clamp01(t);
    let c1 = 1.701_58;
    1.0 + (c1 + 1.0) * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

/// A deterministic value in `[0, 1)` per cell and tick: the flicker is the
/// same on every run, so a frame can be asserted.
fn hash3(a: i32, b: i32, c: i32) -> f32 {
    let mut h = (a as u32).wrapping_mul(0x9E37_79B1)
        ^ (b as u32).wrapping_mul(0x85EB_CA77)
        ^ (c as u32).wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h & 0x00FF_FFFF) as f32 / 16_777_216.0
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = clamp01(t);
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t + 0.5) as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Signed distance from `(px, py)` to a rounded box of half extents
/// `(bx, by)` and corner radius `r`: negative inside, zero on the edge.
fn sd_round_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

/// A ramp glyph at `v` in `[0, 1]`, dense at 0.
fn ramp_at(v: f32) -> char {
    RAMP[((clamp01(v) * (RAMP.len() - 1) as f32) as usize).min(RAMP.len() - 1)]
}

/// A ramp glyph picked by a hash in `[0, 1)`.
fn ramp_pick(h: f32) -> char {
    RAMP[((h * RAMP.len() as f32) as usize).min(RAMP.len() - 1)]
}

// ── canvas ──────────────────────────────────────────────────────────────────

/// One terminal cell; `bg: None` is the terminal's own background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

const BLANK: Cell = Cell {
    ch: ' ',
    fg: INK,
    bg: None,
    bold: false,
};

/// The whole terminal as a grid of cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canvas {
    w: i32,
    h: i32,
    cells: Vec<Cell>,
}

impl Canvas {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            w: i32::from(cols),
            h: i32::from(rows),
            cells: vec![BLANK; usize::from(cols) * usize::from(rows)],
        }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        ((0..self.w).contains(&x) && (0..self.h).contains(&y)).then(|| (y * self.w + x) as usize)
    }

    /// The cell at `(x, y)`, `None` off the grid.
    #[must_use]
    pub fn get(&self, x: i32, y: i32) -> Option<Cell> {
        self.index(x, y).map(|i| self.cells[i])
    }

    /// The characters of row `y`, trailing blanks trimmed.
    #[must_use]
    pub fn row_text(&self, y: i32) -> String {
        (0..self.w)
            .filter_map(|x| self.get(x, y))
            .map(|cell| cell.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Draw `ch` keeping the cell's background.
    fn put(&mut self, x: i32, y: i32, ch: char, fg: Rgb, bold: bool) {
        if let Some(i) = self.index(x, y) {
            let bg = self.cells[i].bg;
            self.cells[i] = Cell { ch, fg, bg, bold };
        }
    }

    fn set(&mut self, x: i32, y: i32, cell: Cell) {
        if let Some(i) = self.index(x, y) {
            self.cells[i] = cell;
        }
    }

    /// Draw `s` from `x`, returning the column after it.
    fn text(&mut self, mut x: i32, y: i32, s: &str, fg: Rgb, bold: bool) -> i32 {
        for ch in s.chars() {
            self.put(x, y, ch, fg, bold);
            x += 1;
        }
        x
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(
                    xx,
                    yy,
                    Cell {
                        bg: Some(bg),
                        ..BLANK
                    },
                );
            }
        }
    }
}

/// The centred window every act draws into.
struct Win {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// First and past-the-end text columns.
    cx0: i32,
    cx1: i32,
    log_top: i32,
    log_rows: i32,
    /// The centre cell, where the pixel is born.
    cx: i32,
    cy: i32,
    /// The centre cell's centre in aspect space, where a row counts two
    /// columns (a cell is about twice as tall as wide).
    ax: f32,
    ay: f32,
}

impl Win {
    fn fit(cols: i32, rows: i32) -> Option<Self> {
        let w = (cols - 4).min(88);
        let h = (rows - 2).min(24);
        if w < 60 || h < 16 {
            return None;
        }
        let x = (cols - w) / 2;
        let y = (rows - h) / 2;
        let (cx, cy) = (x + w / 2, y + h / 2);
        Some(Self {
            x,
            y,
            w,
            h,
            cx0: x + 3,
            cx1: x + w - 3,
            log_top: y + 4,
            log_rows: h - 7,
            cx,
            cy,
            ax: cx as f32,
            ay: (cy as f32 + 0.5) * 2.0,
        })
    }

    /// Strictly inside the border.
    fn inside(&self, x: i32, y: i32) -> bool {
        self.x < x && x < self.x + self.w - 1 && self.y < y && y < self.y + self.h - 1
    }

    /// Aspect-space offset of cell `(x, y)` from the centre.
    fn offset(&self, x: i32, y: i32) -> (f32, f32) {
        (x as f32 + 0.5 - self.ax, (y as f32 + 0.5) * 2.0 - self.ay)
    }

    fn status_row(&self) -> i32 {
        self.y + self.h - 2
    }
}

/// Whether a terminal of `cols`×`rows` is big enough for the intro.
#[must_use]
pub fn fits(cols: u16, rows: u16) -> bool {
    Win::fit(i32::from(cols), i32::from(rows)).is_some()
}

fn draw_window(c: &mut Canvas, win: &Win, title: &[(&str, Rgb, bool)], border: Rgb) {
    c.fill(win.x, win.y, win.w, win.h, TERM);
    let (x0, y0, x1, y1) = (win.x, win.y, win.x + win.w - 1, win.y + win.h - 1);
    for x in x0 + 1..x1 {
        c.put(x, y0, '─', border, false);
        c.put(x, y1, '─', border, false);
    }
    for y in y0 + 1..y1 {
        c.put(x0, y, '│', border, false);
        c.put(x1, y, '│', border, false);
    }
    c.put(x0, y0, '╭', border, false);
    c.put(x1, y0, '╮', border, false);
    c.put(x0, y1, '╰', border, false);
    c.put(x1, y1, '╯', border, false);
    let mut x = x0 + 3;
    c.put(x0 + 2, y0, ' ', border, false);
    for &(s, fg, bold) in title {
        x = c.text(x, y0, s, fg, bold);
    }
    c.put(x, y0, ' ', border, false);
}

// ── log lines: what scrolls in a window ─────────────────────────────────────

struct Seg {
    text: String,
    fg: Rgb,
    bold: bool,
}

fn seg(text: impl Into<String>, fg: Rgb, bold: bool) -> Seg {
    Seg {
        text: text.into(),
        fg,
        bold,
    }
}

/// One log row, shown from `at`. `typed` reveals it char by char over that
/// many seconds; `decode` resolves each char out of the ramp, the way
/// pixel's answers materialise.
struct Line {
    at: f32,
    segs: Vec<Seg>,
    typed: f32,
    decode: bool,
}

impl Line {
    fn plain(at: f32, segs: Vec<Seg>) -> Self {
        Self {
            at,
            segs,
            typed: 0.0,
            decode: false,
        }
    }
}

/// `text` with every `word` painted `hit`: what a bare grep matched.
fn highlight(text: &str, word: &str, fg: Rgb, hit: Rgb) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut rest = text;
    while let Some(j) = rest.find(word) {
        if j > 0 {
            segs.push(seg(&rest[..j], fg, false));
        }
        segs.push(seg(word, hit, false));
        rest = &rest[j + word.len()..];
    }
    segs.push(seg(rest, fg, false));
    segs
}

fn draw_line(c: &mut Canvas, win: &Win, y: i32, line: &Line, t: f32, seed: i32) {
    let total = line
        .segs
        .iter()
        .map(|s| s.text.chars().count())
        .sum::<usize>();
    let shown = if line.typed > 0.0 {
        (total as f32 * clamp01((t - line.at) / line.typed)) as usize
    } else {
        total
    };
    let chars = line
        .segs
        .iter()
        .flat_map(|s| s.text.chars().map(move |ch| (ch, s.fg, s.bold)));
    for (i, (ch, fg, bold)) in chars.enumerate().take(shown) {
        let i = i as i32;
        let x = win.cx0 + i;
        if x >= win.cx1 {
            c.put(win.cx1 - 1, y, '…', FAINT, false);
            return;
        }
        let pending = line.decode && ch != ' ' && t < line.at + 0.05 + 0.22 * hash3(i, seed, 0);
        if pending {
            let h = hash3(i, seed, (t * 30.0) as i32);
            c.put(x, y, ramp_pick(h), mix(GREEN_DIM, GREEN, h), false);
        } else {
            c.put(x, y, ch, fg, bold);
        }
    }
}

/// The lines shown at `t`, scrolled so the newest sits on the last row.
fn draw_log(c: &mut Canvas, win: &Win, lines: &[Line], t: f32) {
    let shown: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].at <= t).collect();
    let first = shown.len().saturating_sub(win.log_rows as usize);
    for (row, &i) in shown[first..].iter().enumerate() {
        draw_line(c, win, win.log_top + row as i32, &lines[i], t, i as i32);
    }
}

const TASK: &str = "retry a leased push when the remote branch moved";

/// The task both windows work on, typed over `typed` seconds from `at`
/// (already there when `typed` is zero).
fn draw_prompt(c: &mut Canvas, win: &Win, t: f32, at: f32, typed: f32) {
    let n = if typed > 0.0 {
        (TASK.len() as f32 * clamp01((t - at) / typed)) as usize
    } else {
        TASK.len()
    };
    let mark = if typed > 0.0 { CORAL } else { GREEN };
    c.put(win.cx0, win.y + 2, '›', mark, true);
    c.text(win.cx0 + 2, win.y + 2, &TASK[..n], INK, false);
}

// ── act 1: without pixel ────────────────────────────────────────────────────

/// One recorded call of the vanilla-6 run: tool, argument, the line count
/// it returned, and real output of that shape to scroll.
struct Step {
    tool: &'static str,
    arg: &'static str,
    lines: usize,
    out: &'static [&'static str],
    /// When the call appears; each comes sooner than the last.
    at: f32,
    verb: &'static str,
}

const STEPS: [Step; 11] = [
    Step {
        tool: "Bash",
        arg: r#"grep -rln "lease" --include=*.sh --include=*.ts --include=*.js --include=*.py . | grep -v node_modules"#,
        lines: 1,
        out: &[],
        at: 0.6,
        verb: "Searching",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -rln "force-with-lease\|force_with_lease" --include=*.sh --include=*.ts --include=*.yml ."#,
        lines: 1,
        out: &[],
        at: 0.9,
        verb: "Searching",
    },
    Step {
        tool: "Grep",
        arg: r#""lease""#,
        lines: 122,
        out: data::GREP_LEASE,
        at: 1.2,
        verb: "Grepping",
    },
    Step {
        tool: "Grep",
        arg: r#""force-with-lease|force_with_lease|push.*lease""#,
        lines: 20,
        out: data::GREP_FORCE,
        at: 1.65,
        verb: "Grepping",
    },
    Step {
        tool: "Read",
        arg: "crates/pixel-ops/src/push.rs",
        lines: 591,
        out: data::PUSH_RS,
        at: 2.0,
        verb: "Reading push.rs",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -n "lease\|force" crates/pixel-ops/src/ship.rs crates/pixel-ops/src/reconcile.rs …"#,
        lines: 92,
        out: data::GREP_SHIP,
        at: 2.45,
        verb: "Grepping again",
    },
    Step {
        tool: "Read",
        arg: "crates/pixel-ops/src/reconcile.rs :1230+100",
        lines: 100,
        out: data::RECONCILE_RS,
        at: 2.8,
        verb: "Reading reconcile.rs",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -n "JournalPhase\|enum JournalPhase" crates/pixel-ops/src/journal.rs"#,
        lines: 23,
        out: data::GREP_JOURNAL,
        at: 3.1,
        verb: "Still looking",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -n "push\|Push" crates/pixel-proto/src/op.rs … crates/pixel-daemon/src/api.rs"#,
        lines: 60,
        out: data::GREP_PUSH,
        at: 3.35,
        verb: "Still looking",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -rn "push" crates/pixel-ops/tests/all/crash_matrix.rs"#,
        lines: 37,
        out: data::GREP_CRASH,
        at: 3.57,
        verb: "Re-grepping",
    },
    Step {
        tool: "Bash",
        arg: r#"grep -rn "fn run(\|struct GitError\|stderr" crates/pixel-git/src/*.rs"#,
        lines: 31,
        out: data::GREP_RUN,
        at: 3.77,
        verb: "Wandering",
    },
];

/// Every step's header, its output streaming in, then its line count.
fn scene1_lines() -> Vec<Line> {
    let mut lines = Vec::new();
    for (k, step) in STEPS.iter().enumerate() {
        let next = STEPS.get(k + 1).map_or(S1_END, |s| s.at);
        let at = step.at;
        lines.push(Line::plain(
            at,
            vec![
                seg("● ", CORAL, false),
                seg(step.tool, INK, true),
                seg(format!("({})", step.arg), SOFT, false),
            ],
        ));
        let dt = 0.03_f32.min((next - at - 0.08) * 0.85 / step.out.len().max(1) as f32);
        for (j, text) in step.out.iter().enumerate() {
            let mut segs = vec![seg("     ", NOISE, false)];
            if step.tool == "Read" {
                segs.push(seg(*text, NOISE, false));
            } else {
                segs.extend(highlight(text, "lease", NOISE, CORAL));
            }
            lines.push(Line::plain(at + 0.06 + j as f32 * dt, segs));
        }
        let n = step.lines;
        let summary = match (step.tool, n) {
            ("Read", _) => format!("Read {n} lines"),
            (_, 1) => "1 line".to_string(),
            _ => format!("{n} lines"),
        };
        lines.push(Line::plain(
            at + 0.07 + step.out.len() as f32 * dt,
            vec![seg("  ⎿  ", FAINT, false), seg(summary, SOFT, false)],
        ));
    }
    lines
}

/// The status row: a spinner, a verb that slowly gives up, and the context
/// gauge filling with every line the agent pulled in.
fn draw_struggle_status(c: &mut Canvas, win: &Win, t: f32) {
    let Some(k) = STEPS.iter().rposition(|s| s.at <= t) else {
        return;
    };
    let y = win.status_row();
    c.put(
        win.cx0,
        y,
        SPIN[(t * 12.0) as usize % SPIN.len()],
        CORAL,
        true,
    );
    c.text(
        win.cx0 + 2,
        y,
        &format!("{}…", STEPS[k].verb),
        CORAL_INK,
        false,
    );
    let total: usize = STEPS.iter().map(|s| s.lines).sum();
    let read: usize = STEPS[..=k].iter().map(|s| s.lines).sum();
    let cells = 16;
    let lit = (read * cells as usize).div_ceil(total) as i32;
    let gx = win.cx1 - cells - 10;
    c.text(gx, y, "context ", FAINT, false);
    c.put(gx + 8, y, '▕', LINE, false);
    for i in 0..cells {
        if i < lit {
            c.put(
                gx + 9 + i,
                y,
                '█',
                mix(CORAL_DIM, CORAL, i as f32 / cells as f32),
                false,
            );
        } else {
            c.put(gx + 9 + i, y, '░', LINE, false);
        }
    }
    c.put(gx + 9 + cells, y, '▏', LINE, false);
}

fn scene1(c: &mut Canvas, win: &Win, lines: &[Line], t: f32) {
    draw_window(c, win, &[("without pixel", CORAL, true)], LINE);
    draw_prompt(c, win, t, 0.05, 0.4);
    draw_log(c, win, lines, t);
    draw_struggle_status(c, win, t);
    glitch(c, win, smooth(3.25, S1_END, t), t);
}

/// Characters break into the ramp and rows slip sideways: the agent losing
/// its thread. `g` is the intensity, 0 for none.
#[cfg_attr(test, mutants::skip)] // visual noise: no contract fixes which cells flicker; its end state is covered by `collapse_starts_from_the_struggle`
fn glitch(c: &mut Canvas, win: &Win, g: f32, t: f32) {
    if g <= 0.0 {
        return;
    }
    let f = (t * 24.0) as i32;
    for y in win.log_top..win.log_top + win.log_rows {
        if hash3(y, f, 7) < g * 0.3 {
            let shift = if hash3(y, f, 9) < 0.5 { 1 } else { -1 };
            let row: Vec<Cell> = (win.x + 1..win.x + win.w - 1)
                .filter_map(|x| c.get(x, y))
                .collect();
            for (i, cell) in row.iter().enumerate() {
                let x = win.x + 1 + i as i32 + shift;
                if win.inside(x, y) {
                    c.put(x, y, cell.ch, cell.fg, cell.bold);
                }
            }
        }
        for x in win.x + 1..win.x + win.w - 1 {
            if let Some(cell) = c.get(x, y)
                && cell.ch != ' '
                && hash3(x, y, f) < g * 0.45
            {
                c.put(
                    x,
                    y,
                    ramp_pick(hash3(x, y, f + 1)),
                    mix(cell.fg, CORAL, g),
                    false,
                );
            }
        }
    }
}

// ── act 2: collapse into one pixel, then bloom ──────────────────────────────

/// A lit cell of act 1's last frame, on its way to the centre.
struct Particle {
    x: i32,
    y: i32,
    cell: Cell,
    /// Polar position around the centre, in aspect space.
    r: f32,
    ang: f32,
    /// Seconds before it starts to fall: the outer ones go last.
    delay: f32,
}

#[cfg_attr(test, mutants::skip)] // visual timing: when each cell falls is no contract; the first and last frames are pinned by `collapse_starts_from_the_struggle` and `collapse_absorbs_every_character`
fn particles(cols: u16, rows: u16, win: &Win, lines: &[Line]) -> Vec<Particle> {
    let mut snap = Canvas::new(cols, rows);
    scene1(&mut snap, win, lines, S1_END - 1e-3);
    let maxd = (win.w as f32 / 2.0).hypot(win.h as f32);
    let mut out = Vec::new();
    for y in win.y + 1..win.y + win.h - 1 {
        for x in win.x + 1..win.x + win.w - 1 {
            let Some(cell) = snap.get(x, y).filter(|cell| cell.ch != ' ') else {
                continue;
            };
            let (dx, dy) = win.offset(x, y);
            let r = dx.hypot(dy);
            out.push(Particle {
                x,
                y,
                cell,
                r,
                ang: dy.atan2(dx),
                delay: 0.38 * (r / maxd) + 0.14 * hash3(x, y, 3),
            });
        }
    }
    out
}

/// Half the logo pixel's side, in aspect units: 6 columns by 3 rows.
const PIXEL_HALF: f32 = 3.0;

/// A shaded disc at the centre, dense in the middle; `bright` lifts its rim
/// towards the core's colour.
#[cfg_attr(test, mutants::skip)] // visual shading: its result is pinned by `the_pixel_pulses_at_the_centre`
fn draw_core(c: &mut Canvas, win: &Win, radius: f32, t: f32, bright: f32) {
    if radius <= 0.2 {
        return;
    }
    let reach = radius as i32 + 1;
    for y in win.cy - reach / 2 - 1..=win.cy + reach / 2 + 1 {
        for x in win.cx - reach..=win.cx + reach {
            let (dx, dy) = win.offset(x, y);
            let d = dx.hypot(dy);
            if d < radius {
                let v = d / radius;
                let jitter = (hash3(x, y, (t * 30.0) as i32) - 0.5) * 0.25;
                c.put(
                    x,
                    y,
                    ramp_at(v + jitter),
                    mix(GREEN_HI, GREEN_DIM, v * (1.0 - bright)),
                    true,
                );
            }
        }
    }
}

/// The noise spirals into the centre, thinning through the ramp and
/// turning green, and the centre swells, then condenses to the pixel.
#[cfg_attr(test, mutants::skip)] // visual easing: the spiral's shape is no contract; both ends are pinned by `collapse_starts_from_the_struggle` and `collapse_absorbs_every_character`
fn collapse(c: &mut Canvas, win: &Win, parts: &[Particle], t: f32) {
    let u0 = t - S1_END;
    let p = clamp01(u0 / (A_END - S1_END));
    let fade = smooth(0.0, 0.6, p);
    draw_window(
        c,
        win,
        &[("without pixel", mix(CORAL, TERM, fade), true)],
        mix(LINE, TERM, fade),
    );
    let mut absorbed = 0;
    for part in parts {
        let u = clamp01((u0 - part.delay) / 0.55);
        if u >= 1.0 {
            absorbed += 1;
            continue;
        }
        if u <= 0.0 {
            c.set(part.x, part.y, part.cell);
            continue;
        }
        let e = u * u * u;
        let r = part.r * (1.0 - e);
        let a = part.ang + 2.4 * e;
        let nx = (win.ax + r * a.cos()) as i32;
        let ny = ((win.ay + r * a.sin()) / 2.0) as i32;
        let glyph = if u < 0.12 {
            part.cell.ch
        } else {
            ramp_pick((u - 0.12) / 0.88)
        };
        c.put(
            nx,
            ny,
            glyph,
            mix(part.cell.fg, GREEN_HI, smooth(0.08, 0.6, u)),
            false,
        );
    }
    let frac = absorbed as f32 / parts.len().max(1) as f32;
    let shrink = smooth(0.82, 1.0, p);
    draw_core(
        c,
        win,
        (1.5 + 5.5 * frac) * (1.0 - shrink) + PIXEL_HALF * shrink,
        t,
        shrink,
    );
}

/// The logo pixel's cells: 6 columns by 3 rows around the centre.
fn pixel_cells(win: &Win) -> impl Iterator<Item = (i32, i32)> + '_ {
    (win.cy - 1..=win.cy + 1).flat_map(move |y| (win.cx - 3..win.cx + 3).map(move |x| (x, y)))
}

/// The pixel flashes white to green, breathes a halo and sends rounded
/// square rings racing to the window's edge.
#[cfg_attr(test, mutants::skip)] // visual easing: ring speed and halo shading are no contract; the pixel itself is pinned by `the_pixel_pulses_at_the_centre`
fn pulse(c: &mut Canvas, win: &Win, t: f32) {
    let tb = t - A_END;
    draw_window(c, win, &[], TERM);
    for k in 0..3 {
        let rr = (tb - k as f32 * 0.13) * 70.0;
        let fade = 1.0 - clamp01(rr / (win.w as f32 * 0.7));
        if rr <= 0.0 || fade <= 0.0 {
            continue;
        }
        for y in win.y + 1..win.y + win.h - 1 {
            for x in win.x + 1..win.x + win.w - 1 {
                let (dx, dy) = win.offset(x, y);
                let g = (sd_round_box(dx, dy, PIXEL_HALF, PIXEL_HALF, 1.5) - rr).abs();
                if g < 2.2 {
                    let v = g / 2.2;
                    let ch = RAMP[(6 + (v * 4.0) as usize).min(RAMP.len() - 1)];
                    c.put(x, y, ch, mix(TERM, mix(GREEN, GREEN_DIM, v), fade), false);
                }
            }
        }
    }
    let breath = 0.5 + 0.5 * (tb * 14.0).sin();
    for y in win.cy - 3..=win.cy + 3 {
        for x in win.cx - 7..win.cx + 7 {
            let (dx, dy) = win.offset(x, y);
            let d = sd_round_box(dx, dy, PIXEL_HALF, PIXEL_HALF, 0.8);
            if d > 0.0 && d < 3.2 {
                let ch = if d < 1.1 {
                    '▓'
                } else if d < 2.2 {
                    '▒'
                } else {
                    '░'
                };
                c.put(
                    x,
                    y,
                    ch,
                    mix(TERM, GREEN_DIM, (1.0 - d / 3.2) * (0.55 + 0.45 * breath)),
                    false,
                );
            }
        }
    }
    let flash = 1.0 - smooth(0.0, 0.18, tb);
    for (x, y) in pixel_cells(win) {
        c.put(x, y, '█', mix(GREEN, INK, flash * 0.85), false);
    }
}

/// The pixel grows into the new window: a rounded square whose rim is the
/// ramp, dense at the edge, revealing act 3 behind it.
#[cfg_attr(test, mutants::skip)] // visual easing: the rim's shading is no contract; the reveal is pinned by `bloom_ends_on_the_pixel_window`
fn bloom(c: &mut Canvas, win: &Win, fresh: &Canvas, t: f32) {
    let e = ease_in_out((t - B_END) / (C_END - B_END));
    let band = 7.0;
    let bx = PIXEL_HALF + (win.w as f32 / 2.0 + band + 2.0 - PIXEL_HALF) * e;
    let by = PIXEL_HALF + (win.h as f32 + band + 2.0 - PIXEL_HALF) * e;
    let rad = (bx.min(by) * 0.35).clamp(1.2, 7.0);
    draw_window(c, win, &[], TERM);
    let f = (t * 30.0) as i32;
    for y in win.y..win.y + win.h {
        for x in win.x..win.x + win.w {
            let (dx, dy) = win.offset(x, y);
            let d = sd_round_box(dx, dy, bx, by, rad);
            let Some(cell) = fresh.get(x, y) else {
                continue;
            };
            if d <= -band {
                c.set(x, y, cell);
            } else if d <= 0.0 {
                let v = -d / band;
                if v > 0.6 && cell.ch != ' ' && hash3(x, y, 11) < v {
                    c.set(x, y, cell);
                } else {
                    let jitter = (hash3(x, y, f) - 0.5) * 0.35;
                    c.set(
                        x,
                        y,
                        Cell {
                            ch: ramp_at(v + jitter),
                            fg: mix(GREEN_HI, GREEN_DIM, v),
                            bg: Some(TERM),
                            bold: v < 0.25,
                        },
                    );
                }
            } else if d < 2.5 && win.inside(x, y) {
                c.put(x, y, '░', mix(TERM, GREEN_DIM, 1.0 - d / 2.5), false);
            }
        }
    }
}

// ── act 3: with pixel ───────────────────────────────────────────────────────

fn cmd(at: f32, verb: &str, arg: &str) -> Line {
    Line {
        at,
        segs: vec![
            seg("● ", GREEN, true),
            seg("pixel ", GREEN_HI, true),
            seg(format!("{verb} "), INK, true),
            seg(arg, SOFT, false),
        ],
        typed: 0.35,
        decode: false,
    }
}

fn answer(at: f32, segs: Vec<Seg>) -> Line {
    Line {
        at,
        segs,
        typed: 0.0,
        decode: true,
    }
}

fn rank(tag: &str, path: &str, syms: &str, first: bool) -> Vec<Seg> {
    let lead = if first { "  ⎿  " } else { "     " };
    let tag = match tag {
        "" => seg("    ", INK, false),
        "P0" => seg("P0  ", GREEN, true),
        other => seg(format!("{other}  "), SOFT, true),
    };
    vec![
        seg(lead, FAINT, false),
        tag,
        seg(format!("{path:<36}"), INK, false),
        seg(syms, SOFT, false),
    ]
}

/// What this repository's index answers for the task (`pixel scope-task`,
/// `pixel who-calls`, `pixel pack-context`), trimmed to the window.
fn scene3_lines() -> Vec<Line> {
    vec![
        cmd(0.15, "scope-task", &format!("\"{TASK}\"")),
        answer(
            0.62,
            rank(
                "P0",
                "crates/pixel-ops/src/push.rs",
                "PushOptions · build_push_args",
                true,
            ),
        ),
        answer(
            0.67,
            rank("", "crates/pixel-ops/src/branch.rs", "branch", false),
        ),
        answer(
            0.72,
            rank("", "crates/pixel/src/decide_remote.rs", "Remote", false),
        ),
        answer(
            0.77,
            rank("", "crates/pixel-ops/src/journal.rs", "PushStarted", false),
        ),
        answer(
            0.82,
            rank(
                "P1",
                "crates/pixel/src/main.rs",
                "CommitAndPush · decide_remote",
                false,
            ),
        ),
        Line::plain(0.9, vec![]),
        cmd(1.15, "who-calls", "push_with_state --role callers"),
        answer(
            1.6,
            vec![
                seg("  ⎿  ", FAINT, false),
                seg("[exact]     ", GREEN, true),
                seg("push                 ", INK, false),
                seg("crates/pixel-ops/src/push.rs:129", SOFT, false),
            ],
        ),
        answer(
            1.65,
            vec![
                seg("     ", FAINT, false),
                seg("[probable]  ", SOFT, true),
                seg("push_crash_at_phase  ", INK, false),
                seg(
                    "crates/pixel-ops/tests/all/crash_matrix.rs:580",
                    SOFT,
                    false,
                ),
            ],
        ),
        Line::plain(1.7, vec![]),
        cmd(
            1.95,
            "pack-context",
            "crates/pixel-ops/src/push.rs#push_with_state#function",
        ),
        answer(
            2.45,
            vec![
                seg("  ⎿  ", FAINT, false),
                seg("push.rs:132-229  ", GREEN, true),
                seg("pub fn ", CORAL_INK, false),
                seg("push_with_state", INK, true),
                seg("(root: &Path, opts: &PushOptions, …)", SOFT, false),
            ],
        ),
        answer(
            2.5,
            vec![
                seg("                      ", FAINT, false),
                seg(
                    "let outcome = journal.begin(&opts.request_id, JournalOperation::Push, …)?;",
                    SOFT,
                    false,
                ),
            ],
        ),
        Line::plain(2.6, vec![]),
        Line {
            at: 2.85,
            segs: vec![
                seg("✓ ", GREEN_HI, true),
                seg("context ready", GREEN_HI, true),
                seg(
                    "  ranked files · their callers · the function itself",
                    SOFT,
                    false,
                ),
            ],
            typed: 0.3,
            decode: false,
        },
    ]
}

fn scene3(c: &mut Canvas, win: &Win, lines: &[Line], t3: f32) {
    draw_window(
        c,
        win,
        &[("■ ", GREEN, true), ("with pixel", GREEN_HI, true)],
        GREEN_DIM,
    );
    draw_prompt(c, win, t3, 0.0, 0.0);
    draw_log(c, win, lines, t3);
    let y = win.status_row();
    c.put(win.cx0, y, '■', GREEN, true);
    c.text(win.cx0 + 2, y, "pixel", GREEN_HI, true);
    c.text(
        win.cx0 + 8,
        y,
        "indexed · local · deterministic",
        FAINT,
        false,
    );
}

// ── act 4: wordmark ─────────────────────────────────────────────────────────

/// "PIXEL" in a five-row pixel face; `#` is a lit pixel, two columns wide.
const GLYPHS: [&[&str; 5]; 5] = [
    &["####.", "#...#", "####.", "#....", "#...."],
    &["###", ".#.", ".#.", ".#.", "###"],
    &["#...#", ".#.#.", "..#..", ".#.#.", "#...#"],
    &["####", "#...", "###.", "#...", "####"],
    &["#...", "#...", "#...", "#...", "####"],
];

/// The lit pixels of the wordmark, and its width in pixels.
fn wordmark() -> (Vec<(i32, i32)>, i32) {
    let mut pixels = Vec::new();
    let mut gx = 0;
    for rows in GLYPHS {
        for (gy, row) in rows.iter().enumerate() {
            for (i, bit) in row.chars().enumerate() {
                if bit == '#' {
                    pixels.push((gx + i as i32, gy as i32));
                }
            }
        }
        gx += rows[0].len() as i32 + 1;
    }
    (pixels, gx - 1)
}

const TAGLINE: &str = "the context your agent needs, straight from the index";
const SETTING_UP: &str = "› setting up pixel…";

/// The pixel window dissolves through the ramp; PIXEL drops in pixel by
/// pixel with a bounce and a shadow, a shimmer runs across it and a sheen
/// sweeps once; the tagline types under it.
#[cfg_attr(test, mutants::skip)] // visual easing: drop, shimmer and sheen are no contract; the settled wordmark is pinned by `the_wordmark_settles_whole`
fn outro(c: &mut Canvas, win: &Win, lines: &[Line], t: f32) {
    let t4 = t - S3_END;
    scene3(c, win, lines, S3_END - C_END);
    let fade = smooth(0.0, 0.5, t4);
    for y in win.y + 1..win.y + win.h - 1 {
        for x in win.x + 1..win.x + win.w - 1 {
            let Some(cell) = c.get(x, y).filter(|cell| cell.ch != ' ') else {
                continue;
            };
            let h = hash3(x, y, 21) * 0.8;
            if fade > h + 0.2 {
                c.put(x, y, ' ', cell.fg, false);
            } else if fade > h {
                let glyph = ramp_pick(hash3(x, y, (t * 30.0) as i32));
                c.put(x, y, glyph, mix(GREEN_DIM, TERM, (fade - h) / 0.2), false);
            }
        }
    }
    let (pixels, width) = wordmark();
    let wx = win.cx - width;
    let wy = win.cy - 4;
    let mut landed = Vec::new();
    for &(px, py) in &pixels {
        let delay = 0.35 + px as f32 * 0.028 + hash3(px, py, 5) * 0.08;
        let u = clamp01((t4 - delay) / 0.4);
        if u <= 0.0 {
            continue;
        }
        let y = (wy as f32 + py as f32 - (1.0 - ease_out_back(u)) * 9.0).round() as i32;
        let x = wx + px * 2;
        if u < 1.0 {
            for k in 1..=2 {
                if win.inside(x, y - k) {
                    let trail = mix(TERM, GREEN_DIM, 0.6 / k as f32);
                    c.put(x, y - k, '░', trail, false);
                    c.put(x + 1, y - k, '░', trail, false);
                }
            }
        }
        landed.push((px, py, x, y, u));
    }
    for &(_, _, x, y, u) in &landed {
        if u >= 1.0
            && win.inside(x + 2, y + 1)
            && c.get(x + 2, y + 1).is_some_and(|cell| cell.ch != '█')
        {
            c.put(x + 2, y + 1, '▒', mix(TERM, GREEN_DIM, 0.35), false);
        }
    }
    let sweep = (t4 - 1.55) * 70.0;
    for &(px, py, x, y, _) in &landed {
        let wave = 0.5 + 0.5 * (0.42 * (px * 2 + py * 3) as f32 - 7.0 * t4).sin();
        let mut col = mix(GREEN_DIM, GREEN_HI, 0.35 + 0.65 * wave);
        let band = ((px * 2 - py * 2) as f32 - sweep).abs();
        if band < 4.0 {
            col = mix(col, INK, 0.75 * (1.0 - band / 4.0));
        }
        if win.inside(x, y) {
            c.put(x, y, '█', col, false);
            c.put(x + 1, y, '█', col, false);
        }
    }
    let n = (TAGLINE.chars().count() as f32 * clamp01((t4 - 1.3) / 0.5)) as usize;
    let tag: String = TAGLINE.chars().take(n).collect();
    c.text(
        win.cx - TAGLINE.chars().count() as i32 / 2,
        wy + 7,
        &tag,
        SOFT,
        false,
    );
    if t4 > 2.2 {
        c.text(win.cx - 9, wy + 9, SETTING_UP, FAINT, false);
    }
}

// ── the whole intro ─────────────────────────────────────────────────────────

/// Which act draws the frame at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Struggle,
    Collapse,
    Pulse,
    Bloom,
    Answer,
    Wordmark,
}

/// The act on screen at `t`; each starts at its predecessor's end.
fn act(t: f32) -> Act {
    if t < S1_END {
        Act::Struggle
    } else if t < A_END {
        Act::Collapse
    } else if t < B_END {
        Act::Pulse
    } else if t < C_END {
        Act::Bloom
    } else if t < S3_END {
        Act::Answer
    } else {
        Act::Wordmark
    }
}

/// The intro's script: both windows' lines, built once, and act 1's last
/// frame broken into particles, cached per terminal size.
pub struct Intro {
    scene1: Vec<Line>,
    scene3: Vec<Line>,
    particles: Option<((u16, u16), Vec<Particle>)>,
}

impl Default for Intro {
    fn default() -> Self {
        Self::new()
    }
}

impl Intro {
    #[must_use]
    pub fn new() -> Self {
        Self {
            scene1: scene1_lines(),
            scene3: scene3_lines(),
            particles: None,
        }
    }

    /// The picture at `t` seconds on a `cols`×`rows` terminal, `None` when
    /// it does not [`fits`].
    pub fn frame(&mut self, cols: u16, rows: u16, t: f32) -> Option<Canvas> {
        let win = Win::fit(i32::from(cols), i32::from(rows))?;
        let mut c = Canvas::new(cols, rows);
        match act(t) {
            Act::Struggle => scene1(&mut c, &win, &self.scene1, t),
            Act::Collapse => {
                if self
                    .particles
                    .as_ref()
                    .is_none_or(|(size, _)| *size != (cols, rows))
                {
                    self.particles =
                        Some(((cols, rows), particles(cols, rows, &win, &self.scene1)));
                }
                let parts = self
                    .particles
                    .as_ref()
                    .map_or(&[][..], |(_, p)| p.as_slice());
                collapse(&mut c, &win, parts, t);
            }
            Act::Pulse => pulse(&mut c, &win, t),
            Act::Bloom => {
                let mut fresh = Canvas::new(cols, rows);
                scene3(&mut fresh, &win, &self.scene3, 0.0);
                bloom(&mut c, &win, &fresh, t);
            }
            Act::Answer => scene3(&mut c, &win, &self.scene3, t - C_END),
            Act::Wordmark => outro(&mut c, &win, &self.scene3, t),
        }
        Some(c)
    }
}

// ── terminal bytes ──────────────────────────────────────────────────────────

/// The nearest xterm-256 cube colour, for terminals without 24-bit colour.
fn to_256(c: Rgb) -> u8 {
    let q = |v: u8| match v {
        0..48 => 0,
        48..115 => 1,
        _ => (v - 35) / 40,
    };
    16 + 36 * q(c.0) + 6 * q(c.1) + q(c.2)
}

fn push_fg(out: &mut String, c: Rgb, truecolor: bool) {
    let _ = if truecolor {
        write!(out, "\x1b[38;2;{};{};{}m", c.0, c.1, c.2)
    } else {
        write!(out, "\x1b[38;5;{}m", to_256(c))
    };
}

fn push_bg(out: &mut String, bg: Option<Rgb>, truecolor: bool) {
    let _ = match bg {
        None => write!(out, "\x1b[49m"),
        Some(c) if truecolor => write!(out, "\x1b[48;2;{};{};{}m", c.0, c.1, c.2),
        Some(c) => write!(out, "\x1b[48;5;{}m", to_256(c)),
    };
}

/// The bytes that turn `prev` into `cur` on screen: only the changed cells,
/// inside one synchronized update so the terminal never shows half a frame.
/// `prev: None` (the first frame, or after a resize) paints every cell.
#[must_use]
pub fn diff(cur: &Canvas, prev: Option<&Canvas>, truecolor: bool) -> String {
    let prev = prev.filter(|p| p.w == cur.w && p.h == cur.h);
    let mut out = String::from("\x1b[?2026h");
    let mut pos = None;
    let (mut fg, mut bg, mut bold) = (None, None, None);
    for y in 0..cur.h {
        for x in 0..cur.w {
            let i = (y * cur.w + x) as usize;
            let cell = cur.cells[i];
            if prev.is_some_and(|p| p.cells[i] == cell) {
                continue;
            }
            if pos != Some((x, y)) {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            }
            if bold != Some(cell.bold) {
                out.push_str(if cell.bold { "\x1b[1m" } else { "\x1b[22m" });
                bold = Some(cell.bold);
            }
            if fg != Some(cell.fg) {
                push_fg(&mut out, cell.fg, truecolor);
                fg = Some(cell.fg);
            }
            if bg != Some(cell.bg) {
                push_bg(&mut out, cell.bg, truecolor);
                bg = Some(cell.bg);
            }
            out.push(cell.ch);
            pos = Some((x + 1, y));
        }
    }
    out.push_str("\x1b[0m\x1b[?2026l");
    out
}

#[cfg(test)]
mod tests;
