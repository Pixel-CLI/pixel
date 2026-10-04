// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The "a newer pixel is released" line, once a day, on a terminal only.
//!
//! The "a newer pixel is released" line, once a day, on a terminal only —
//! and, when stdin is a terminal too, the question under it: update now
//! and relaunch? A yes runs the upgrade command and replaces the process
//! with the fresh binary on the same arguments.
//!
//! Pixel ships often and most installs are never updated by hand, so the
//! binary says when it is behind. Most invocations come from hooks and
//! agents, whose stderr lands in a model's context, so the line is printed
//! only when stderr is a terminal a person reads, never on a protected
//! stream, never under `CI`, and never with `PIXEL_NO_UPDATE_CHECK` set.
//!
//! The check never slows a command down by more than [`JOIN_WAIT`], once a
//! day: the last answer is kept in `$XDG_CACHE_HOME/pixel/release-check.json`
//! (else `~/.cache/pixel/`), a refresh is claimed in that file before the
//! request leaves, so a failed or interrupted one waits a whole
//! [`CHECK_INTERVAL_SECS`] before the next try, and the request runs on a
//! thread while the command works. The only data sent is one `HEAD` to the
//! GitHub releases page, the same request `install.sh` makes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{ManagedRoot, is_developer_build};

/// Seconds between two release checks, and between two notices.
pub(crate) const CHECK_INTERVAL_SECS: u64 = 86_400;
/// Cap on the one request a check makes.
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the end of a command waits for the check its start launched.
/// A command that ends sooner leaves the answer to the next day's check.
const JOIN_WAIT: Duration = Duration::from_millis(1_500);
/// Redirects to `…/releases/tag/<tag>` of the latest non-pre-release.
const RELEASES_LATEST_URL: &str = "https://github.com/Pixel-CLI/pixel/releases/latest";
/// What a non-managed install runs to update: the documented installer.
const INSTALL_SH_COMMAND: &str =
    "curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh";
/// The state file's name under the cache directory.
const STATE_FILE: &str = "release-check.json";
/// The opt-out variable: any value but empty or `0` turns the check off.
pub(crate) const OPT_OUT_VAR: &str = "PIXEL_NO_UPDATE_CHECK";
/// `PIXEL_UPDATE_CHECK` as the build saw it. A package manager that owns
/// updates builds with `PIXEL_UPDATE_CHECK=off` (the homebrew-core formula
/// does): that binary never checks, never prints the notice and never offers
/// to upgrade itself, which such a manager refuses in software it packages.
const BUILT_UPDATE_CHECK: Option<&str> = option_env!("PIXEL_UPDATE_CHECK");

/// Whether the build turned the check off for good: exactly `off`.
fn built_without_update_check(value: Option<&str>) -> bool {
    value == Some("off")
}

/// What the state file keeps between invocations.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    /// Unix seconds of the last check that was started.
    #[serde(default)]
    pub checked_at: u64,
    /// The latest release tag the last successful check saw.
    #[serde(default)]
    pub latest: Option<String>,
    /// Unix seconds of the last notice printed.
    #[serde(default)]
    pub notified_at: u64,
}

/// `major.minor.patch` of a release tag or a crate version, a leading `v`
/// allowed. A pre-release (`1.0.0-rc.1`) or anything else is `None`: the
/// notice only ever points at a stable release.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Whether `latest` is a stable release above `current`. An unparsable
/// side is never newer: a local `0.6.0-dev` build is left alone.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The tag in the URL `releases/latest` redirected to, or `None` when it
/// landed anywhere else (a repository with no release lands on `/releases`).
fn tag_from_url(url: &str) -> Option<&str> {
    let (_, tag) = url.rsplit_once("/releases/tag/")?;
    (!tag.is_empty() && !tag.contains('/')).then_some(tag)
}

/// Whether a full interval has passed since `last`. A clock set back
/// before `last` counts as due, or the check would stop until it caught up.
fn due(last: u64, now: u64) -> bool {
    now < last || now - last >= CHECK_INTERVAL_SECS
}

/// Whether this invocation may check and print: the build kept the check, a
/// person reads its stderr, it is not a protected stream, and neither `CI`
/// nor the opt-out is set. `self-update` is left out: it is how a source
/// build gets updated.
pub(crate) fn enabled(
    protected: bool,
    command_label: &str,
    stderr_is_terminal: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty() && v != "0");
    !built_without_update_check(BUILT_UPDATE_CHECK)
        && !protected
        && stderr_is_terminal
        && command_label != "self-update"
        && !set("CI")
        && !set(OPT_OUT_VAR)
}

/// The state file: `$XDG_CACHE_HOME/pixel/`, else `$HOME/.cache/pixel/`.
pub(crate) fn state_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let base = env("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("pixel").join(STATE_FILE))
}

/// The saved state; a missing or unreadable file is a first run.
fn load(path: &Path) -> State {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write the state through a sibling temporary file and a rename, so a
/// concurrent reader sees the old file or the new one, never half of one.
fn store(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(state)?)?;
    std::fs::rename(&tmp, path)
}

/// Single-quote `text` for a POSIX shell, fish and zsh alike.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command that updates the binary at `exe`, or `None` for a build a
/// developer runs from a checkout (`target/…`, `pixel-dev`), which the
/// notice leaves alone. A mise or Homebrew install gets its manager's
/// command; anything else gets `install.sh`, told where to write when the
/// binary lives outside its default `~/.local/bin`.
pub(crate) fn upgrade_hint(exe: &Path, roots: &[ManagedRoot], home: &Path) -> Option<String> {
    if is_developer_build(exe) {
        return None;
    }
    if let Some(owner) = roots.iter().find(|r| exe.starts_with(&r.root)) {
        return Some(owner.manager.upgrade_command().to_string());
    }
    let dir = exe.parent()?;
    if dir == home.join(".local/bin") {
        return Some(INSTALL_SH_COMMAND.to_string());
    }
    Some(format!(
        "curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | PIXEL_INSTALL_DIR={} sh",
        shell_quote(&dir.display().to_string())
    ))
}

/// The notice itself: one line, yellow unless `color` is off.
fn notice_line(latest: &str, current: &str, hint: &str, color: bool) -> String {
    let latest = latest.strip_prefix('v').unwrap_or(latest);
    let text = format!("pixel {latest} is available (this is {current}) · {hint}");
    if color {
        format!("\x1b[33m{text}\x1b[0m\n")
    } else {
        format!("{text}\n")
    }
}

/// A check in flight between the start and the end of one command.
pub(crate) struct Check {
    path: PathBuf,
    state: State,
    started: Instant,
    answer: Option<Receiver<Option<String>>>,
}

/// Start the day's check: load the state and, when a check is due, claim
/// it in the file and run `fetch` on a thread.
pub(crate) fn begin(
    path: PathBuf,
    now: u64,
    fetch: impl FnOnce() -> Option<String> + Send + 'static,
) -> Check {
    let mut state = load(&path);
    let mut answer = None;
    if due(state.checked_at, now) {
        state.checked_at = now;
        // Claimed before the request leaves: a check that fails, hangs or
        // outlives the process waits a full interval, never retries per call.
        if store(&path, &state).is_ok() {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(fetch());
            });
            answer = Some(rx);
        }
    }
    Check {
        path,
        state,
        started: Instant::now(),
        answer,
    }
}

/// End the check: take the answer if it came within [`JOIN_WAIT`] of the
/// start, and return the notice when a newer release is known and no
/// notice went out during the last interval.
pub(crate) fn finish(
    mut check: Check,
    now: u64,
    current: &str,
    hint: impl FnOnce() -> Option<String>,
    color: bool,
) -> Option<String> {
    let before = check.state.clone();
    if let Some(rx) = &check.answer {
        let wait = JOIN_WAIT.saturating_sub(check.started.elapsed());
        if let Ok(Some(tag)) = rx.recv_timeout(wait) {
            check.state.latest = Some(tag);
        }
    }
    let notice = check
        .state
        .latest
        .as_deref()
        .filter(|latest| is_newer(latest, current) && due(check.state.notified_at, now))
        .and_then(|latest| Some(notice_line(latest, current, &hint()?, color)));
    if notice.is_some() {
        check.state.notified_at = now;
    }
    if check.state != before {
        let _ = store(&check.path, &check.state);
    }
    notice
}

/// The opt-out variable for the update-and-relaunch prompt: any value but
/// empty or `0` keeps the question from being asked.
pub(crate) const PROMPT_OPT_OUT_VAR: &str = "PIXEL_NO_UPDATE_PROMPT";
/// The question under the notice, Enter included, is yes.
const PROMPT: &str = "update now and relaunch? [Y/n] ";

/// Whether this invocation may ask the update question: a person is on the
/// other end of stdin, the command is not the updater itself, and the
/// opt-out is not set. The notice's own gating (terminal stderr, no `CI`,
/// [`OPT_OUT_VAR`]) already ran, or there is no notice to ask about.
fn prompt_enabled(
    command_label: &str,
    stdin_is_terminal: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty() && v != "0");
    stdin_is_terminal && command_label != "self-update" && !set(PROMPT_OPT_OUT_VAR)
}

/// Whether the typed answer accepts the update: yes, `y`, or a bare Enter
/// (the capital `Y` in the prompt marks the default). Anything else,
/// including a read error, declines.
fn accepted(answer: &str) -> bool {
    let answer = answer.trim().to_ascii_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// What `offer` decided after the answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The upgrade ran: main replaces this process with the new binary.
    Relaunch,
    /// Declined, failed, or nothing to offer: main returns normally.
    Stay,
}

/// Ask the question the notice raises, and run the upgrade when taken.
/// The prompt and the child's output go to this process's stderr, the
/// streams `enabled` already checked belong to a person. `ask` reads one
/// answer and `run` executes one upgrade command; both are seams the
/// tests drive in place of the terminal and `sh`.
fn offer(
    hint: Option<String>,
    current: &str,
    ask: &mut dyn FnMut(&str) -> std::io::Result<String>,
    run: &mut dyn FnMut(&str) -> bool,
) -> Outcome {
    let Some(hint) = hint else {
        return Outcome::Stay;
    };
    if !ask(PROMPT).is_ok_and(|answer| accepted(&answer)) {
        return Outcome::Stay;
    }
    eprintln!("updating pixel: {hint}");
    if run(&hint) {
        return Outcome::Relaunch;
    }
    eprintln!("the update failed; staying on pixel {current}");
    Outcome::Stay
}

/// Run the upgrade command through `sh -c` (the Homebrew hint composes two
/// commands with `&&`), the child sharing this process's streams.
pub(crate) fn run_upgrade(command: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .is_ok_and(|status| status.success())
}

/// Replace this process with the freshly upgraded binary on the same
/// arguments: the user continues in the new pixel and nothing of this
/// invocation runs after the swap.
#[cfg_attr(test, mutants::skip)] // exec never returns; nothing in-process can observe it
pub(crate) fn relaunch(exe: &Path, args: &[String]) -> ! {
    let mut command = std::process::Command::new(exe);
    command.args(args);
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    eprintln!("pixel: could not relaunch {}: {error}", exe.display());
    std::process::exit(1);
}

/// What the finished command hands the update close: the notice already on
/// stderr, the exit code a command owns (never asked when set), the label
/// and stdin state the prompt gates on, whether the command may re-run
/// (the relaunch executes it again), the upgrade command for the running
/// binary, its stable executable path — one that survives the upgrade, a
/// PATH symlink, not a versioned store path — and the invocation to
/// continue it with.
pub(crate) struct Close<'a> {
    pub(crate) notice: Option<&'a str>,
    pub(crate) owned_exit: Option<i32>,
    pub(crate) command_label: &'a str,
    pub(crate) stdin_is_terminal: bool,
    pub(crate) read_only: bool,
    pub(crate) hint: Option<String>,
    pub(crate) exe: Option<PathBuf>,
    pub(crate) args: &'a [String],
}

/// The close of a command that saw an update notice: re-check every gate
/// (`offer`'s question must never fire for a hook, an agent, CI, the
/// updater itself, a command that owns its exit code, or a command whose
/// re-run would mutate state), ask it, run the upgrade on a yes, and hand
/// `start` the binary to launch. On a taken update `start` never returns;
/// everything else comes back to main.
pub(crate) fn close_with_update(
    close: Close,
    env: impl Fn(&str) -> Option<OsString>,
    ask: &mut dyn FnMut(&str) -> std::io::Result<String>,
    run: &mut dyn FnMut(&str) -> bool,
    start: &mut dyn FnMut(&Path, &[String]),
) {
    if close.notice.is_none()
        || close.owned_exit.is_some()
        || !close.read_only
        || !prompt_enabled(close.command_label, close.stdin_is_terminal, &env)
    {
        return;
    }
    if offer(close.hint, env!("CARGO_PKG_VERSION"), ask, run) == Outcome::Relaunch
        && let Some(exe) = close.exe
    {
        start(&exe, close.args);
    }
}

/// The latest release tag, from where GitHub's `releases/latest` redirects.
#[cfg_attr(test, mutants::skip)] // thin adapter over ureq; the URL parsing is tested
pub(crate) fn fetch_latest_tag() -> Option<String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent(concat!("pixel-cli/", env!("CARGO_PKG_VERSION")))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let response = agent.head(RELEASES_LATEST_URL).call().ok()?;
    let url = ureq::ResponseExt::get_uri(&response).to_string();
    tag_from_url(&url).map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManagedBy;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-upd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("pixel").join(STATE_FILE)
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn parse_version_should_take_stable_triples_only() {
        assert_eq!(parse_version("0.5.2"), Some((0, 5, 2)));
        assert_eq!(parse_version("v12.0.31"), Some((12, 0, 31)));
        assert_eq!(parse_version("1.0.0-rc.1"), None);
        assert_eq!(parse_version("1.0"), None);
        assert_eq!(parse_version("1.0.0.1"), None);
        assert_eq!(parse_version("vx.1.2"), None);
    }

    #[test]
    fn is_newer_should_compare_numerically_and_refuse_unparsable_sides() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.5.2", "0.5.2"));
        assert!(!is_newer("v0.5.1", "0.5.2"));
        assert!(!is_newer("v0.6.0-rc.1", "0.5.2"));
        assert!(!is_newer("v0.6.0", "0.6.0-dev"));
    }

    #[test]
    fn tag_from_url_should_read_the_redirect_target_only() {
        assert_eq!(
            tag_from_url("https://github.com/Pixel-CLI/pixel/releases/tag/v0.5.2"),
            Some("v0.5.2")
        );
        assert_eq!(
            tag_from_url("https://github.com/Pixel-CLI/pixel/releases"),
            None
        );
        assert_eq!(tag_from_url("https://github.com/x/releases/tag/"), None);
        assert_eq!(
            tag_from_url("https://github.com/x/releases/tag/v1/extra"),
            None
        );
    }

    #[test]
    fn due_should_wait_one_full_interval_and_survive_a_clock_set_back() {
        assert!(!due(1_000, 1_000));
        assert!(!due(1_000, 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(due(1_000, 1_000 + CHECK_INTERVAL_SECS));
        assert!(due(1_000, 999));
        assert!(due(0, CHECK_INTERVAL_SECS));
    }

    /// Only `off` removes the check: a packager's typo (`of`, `0`, empty)
    /// must not silently leave a binary that still offers to update itself,
    /// nor may an unrelated value take the notice away from everyone else.
    #[test]
    fn built_without_update_check_should_take_exactly_off() {
        assert!(built_without_update_check(Some("off")));
        for kept in [
            None,
            Some(""),
            Some("on"),
            Some("0"),
            Some("OFF"),
            Some("of"),
        ] {
            assert!(!built_without_update_check(kept), "{kept:?}");
        }
        // The test build is an ordinary one: the check is kept.
        assert_eq!(BUILT_UPDATE_CHECK, None);
    }

    #[test]
    fn enabled_should_speak_only_to_a_person_at_a_terminal() {
        let quiet = env_of(&[]);
        assert!(enabled(false, "find-code", true, &quiet));
        // An agent's stderr, a hook, `self-update`: never.
        assert!(!enabled(false, "find-code", false, &quiet));
        assert!(!enabled(true, "run-hook", true, &quiet));
        assert!(!enabled(false, "self-update", true, &quiet));
        assert!(!enabled(
            false,
            "find-code",
            true,
            env_of(&[("CI", "true")])
        ));
        assert!(!enabled(
            false,
            "find-code",
            true,
            env_of(&[(OPT_OUT_VAR, "1")])
        ));
        // Empty and `0` read as unset, the way `PIXEL_METRICS=0` does.
        assert!(enabled(
            false,
            "find-code",
            true,
            env_of(&[(OPT_OUT_VAR, "0")])
        ));
        assert!(enabled(false, "find-code", true, env_of(&[("CI", "")])));
    }

    #[test]
    fn state_path_should_prefer_xdg_cache_home_then_home() {
        assert_eq!(
            state_path(env_of(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])),
            Some(PathBuf::from("/x/pixel/release-check.json"))
        );
        assert_eq!(
            state_path(env_of(&[("XDG_CACHE_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.cache/pixel/release-check.json"))
        );
        assert_eq!(state_path(env_of(&[("HOME", "")])), None);
    }

    #[test]
    fn upgrade_hint_should_name_the_command_that_owns_the_install() {
        let home = Path::new("/home/u");
        let roots = [
            ManagedRoot {
                root: PathBuf::from("/opt/homebrew/Cellar"),
                manager: ManagedBy::Homebrew,
            },
            ManagedRoot {
                root: PathBuf::from("/home/u/.local/share/mise/installs"),
                manager: ManagedBy::Mise,
            },
        ];
        let hint = |exe: &str| upgrade_hint(Path::new(exe), &roots, home);
        assert_eq!(
            hint("/opt/homebrew/Cellar/pixel/0.5.2/bin/pixel").as_deref(),
            Some("brew update && brew upgrade LivioGama/tap/pixel")
        );
        assert_eq!(
            hint("/home/u/.local/share/mise/installs/pixel/0.5.2/pixel").as_deref(),
            Some("mise upgrade pixel")
        );
        assert_eq!(
            hint("/home/u/.local/bin/pixel").as_deref(),
            Some(INSTALL_SH_COMMAND)
        );
        assert_eq!(
            hint("/opt/my tools/pixel").as_deref(),
            Some(
                "curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | PIXEL_INSTALL_DIR='/opt/my tools' sh"
            )
        );
        // A developer's own builds are never nagged.
        assert_eq!(hint("/home/u/code/pixel/target/dev-release/pixel"), None);
        assert_eq!(hint("/home/u/.local/bin/pixel-dev"), None);
    }

    #[test]
    fn shell_quote_should_survive_a_single_quote() {
        assert_eq!(shell_quote("/a b"), "'/a b'");
        assert_eq!(shell_quote("/it's"), r"'/it'\''s'");
    }

    #[test]
    fn notice_line_should_be_one_yellow_line_or_plain_without_color() {
        assert_eq!(
            notice_line("v0.6.0", "0.5.2", "mise upgrade pixel", true),
            "\x1b[33mpixel 0.6.0 is available (this is 0.5.2) · mise upgrade pixel\x1b[0m\n"
        );
        assert_eq!(
            notice_line("v0.6.0", "0.5.2", "mise upgrade pixel", false),
            "pixel 0.6.0 is available (this is 0.5.2) · mise upgrade pixel\n"
        );
    }

    #[test]
    fn a_due_check_should_record_the_tag_and_print_once_per_interval() {
        let path = scratch("once");
        let now = 2_000_000_000;
        let check = begin(path.clone(), now, || Some("v9.0.0".into()));
        let hint = || Some("mise upgrade pixel".to_string());
        assert_eq!(
            finish(check, now, "0.5.2", hint, false).as_deref(),
            Some("pixel 9.0.0 is available (this is 0.5.2) · mise upgrade pixel\n")
        );
        assert_eq!(
            load(&path),
            State {
                checked_at: now,
                latest: Some("v9.0.0".into()),
                notified_at: now,
            }
        );
        // The next command of the day neither fetches nor prints.
        let check = begin(path.clone(), now + 60, || panic!("fetched twice in a day"));
        assert_eq!(finish(check, now + 60, "0.5.2", hint, false), None);
        // A day later it prints again (and checks again).
        let later = now + CHECK_INTERVAL_SECS;
        let check = begin(path.clone(), later, || Some("v9.0.0".into()));
        assert!(finish(check, later, "0.5.2", hint, false).is_some());
        assert_eq!(load(&path).notified_at, later);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_failed_check_should_still_wait_a_full_interval() {
        let path = scratch("fail");
        let now = 2_000_000_000;
        let check = begin(path.clone(), now, || None);
        assert_eq!(
            finish(check, now, "0.5.2", || Some("x".into()), false),
            None
        );
        assert_eq!(
            load(&path),
            State {
                checked_at: now,
                latest: None,
                notified_at: 0,
            }
        );
        let check = begin(path.clone(), now + 1, || {
            panic!("retried within the interval")
        });
        assert_eq!(
            finish(check, now + 1, "0.5.2", || Some("x".into()), false),
            None
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn an_up_to_date_or_developer_binary_should_get_no_notice() {
        let path = scratch("quiet");
        let now = 2_000_000_000;
        let check = begin(path.clone(), now, || Some("v0.5.2".into()));
        assert_eq!(
            finish(check, now, "0.5.2", || Some("x".into()), false),
            None
        );
        assert_eq!(load(&path).notified_at, 0);
        store(
            &path,
            &State {
                checked_at: now,
                latest: Some("v9.0.0".into()),
                notified_at: 0,
            },
        )
        .unwrap();
        // Newer release known, but a checkout build has no hint: silence,
        // and the day's notice is not spent.
        let check = begin(path.clone(), now, || None);
        assert_eq!(finish(check, now, "0.5.2", || None, false), None);
        assert_eq!(load(&path).notified_at, 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_slow_check_should_not_hold_the_command_past_the_join_wait() {
        let path = scratch("slow");
        let now = 2_000_000_000;
        let check = begin(path.clone(), now, || {
            std::thread::sleep(JOIN_WAIT * 4);
            Some("v9.0.0".into())
        });
        let started = Instant::now();
        assert_eq!(
            finish(check, now, "0.5.2", || Some("x".into()), false),
            None
        );
        assert!(started.elapsed() < JOIN_WAIT * 2, "{:?}", started.elapsed());
        // The claim is on disk: the next command does not start another.
        assert_eq!(load(&path).checked_at, now);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_corrupt_state_file_should_read_as_a_first_run() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path), State::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn accepted_should_take_yes_or_a_bare_enter_only() {
        assert!(accepted("\n"));
        assert!(accepted(" \n"));
        assert!(accepted("y\n"));
        assert!(accepted("YES\r\n"));
        assert!(!accepted("n\n"));
        assert!(!accepted("no\n"));
        assert!(!accepted("ok\n"));
    }

    #[test]
    fn prompt_enabled_should_demand_a_tty_and_honour_the_opt_out() {
        let none = env_of(&[]);
        assert!(prompt_enabled("search", true, &none));
        assert!(!prompt_enabled("search", false, &none));
        // The updater must not offer to update itself.
        assert!(!prompt_enabled("self-update", true, &none));
        assert!(!prompt_enabled(
            "search",
            true,
            env_of(&[(PROMPT_OPT_OUT_VAR, "1")])
        ));
        // Empty and `0` stay unset, like `OPT_OUT_VAR`.
        assert!(prompt_enabled(
            "search",
            true,
            env_of(&[(PROMPT_OPT_OUT_VAR, "0")])
        ));
    }

    /// An `ask` that answers from a list and counts its prompts, paired
    /// with the count after the call.
    fn scripted(
        answers: Vec<Result<&str, std::io::Error>>,
    ) -> (
        impl FnMut(&str) -> std::io::Result<String>,
        impl Fn() -> usize,
    ) {
        let mut next = answers.into_iter();
        let asked = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = asked.clone();
        (
            move |prompt| {
                asked.set(asked.get() + 1);
                assert_eq!(prompt, PROMPT);
                next.next().map_or_else(
                    || {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "no scripted answer",
                        ))
                    },
                    |answer| answer.map(String::from),
                )
            },
            move || counter.get(),
        )
    }

    #[test]
    fn offer_should_stay_when_there_is_no_hint() {
        let (mut ask, asked) = scripted(vec![]);
        assert_eq!(
            offer(None, "0.6.0", &mut ask, &mut |_| {
                panic!("no upgrade without a hint")
            }),
            Outcome::Stay
        );
        assert_eq!(asked(), 0);
    }

    #[test]
    fn offer_should_stay_without_running_anything_on_a_decline_or_a_read_error() {
        for answer in [
            Ok("n\n"),
            Err(std::io::Error::other("stdin closed")),
            Ok("maybe\n"),
        ] {
            let (mut ask, asked) = scripted(vec![answer]);
            assert_eq!(
                offer(Some("brew up".into()), "0.6.0", &mut ask, &mut |_| {
                    panic!("a declined update must not run")
                }),
                Outcome::Stay
            );
            assert_eq!(asked(), 1);
        }
    }

    #[test]
    fn offer_should_relaunch_when_the_upgrade_succeeds_and_stay_when_it_fails() {
        let (mut ask, asked) = scripted(vec![Ok("\n")]);
        let commands = std::cell::RefCell::new(Vec::new());
        assert_eq!(
            offer(
                Some("brew update && brew upgrade".into()),
                "0.6.0",
                &mut ask,
                &mut |command: &str| {
                    commands.borrow_mut().push(command.to_string());
                    true
                },
            ),
            Outcome::Relaunch
        );
        assert_eq!(asked(), 1);
        assert_eq!(commands.into_inner(), ["brew update && brew upgrade"]);
        let (mut ask, _) = scripted(vec![Ok("y\n")]);
        assert_eq!(
            offer(Some("brew up".into()), "0.6.0", &mut ask, &mut |_| false),
            Outcome::Stay
        );
    }

    #[test]
    fn run_upgrade_should_report_the_child_status() {
        assert!(run_upgrade("true"));
        assert!(!run_upgrade("exit 3"));
        assert!(!run_upgrade("definitely-not-a-command-px"));
    }

    /// Drive `close_with_update` with a fixed hint/exe and a `start` that
    /// records its arguments; returns the notice state (asked, started).
    #[allow(clippy::too_many_arguments)]
    fn closed(
        update_line: Option<&str>,
        owned_exit: Option<i32>,
        label: &str,
        tty: bool,
        read_only: bool,
        env: impl Fn(&str) -> Option<OsString>,
        answers: Vec<Result<&str, std::io::Error>>,
        upgrade_ok: bool,
    ) -> (usize, Vec<String>) {
        let (mut ask, asked) = scripted(answers);
        let started = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = started.clone();
        let mut run = |_: &str| upgrade_ok;
        let mut start = |exe: &Path, args: &[String]| {
            seen.borrow_mut()
                .push(format!("{} {args:?}", exe.display()));
        };
        close_with_update(
            Close {
                notice: update_line,
                owned_exit,
                command_label: label,
                stdin_is_terminal: tty,
                read_only,
                hint: Some("brew up".into()),
                exe: Some(PathBuf::from("/tmp/px-close-none/bin/pixel")),
                args: &["doctor".to_string()],
            },
            env,
            &mut ask,
            &mut run,
            &mut start,
        );
        (asked(), started.borrow().clone())
    }

    #[test]
    fn close_should_ask_and_hand_over_the_binary_when_the_update_is_taken() {
        let (asked, started) = closed(
            Some("pixel 9"),
            None,
            "search",
            true,
            true,
            env_of(&[]),
            vec![Ok("y\n")],
            true,
        );
        assert_eq!(asked, 1);
        assert_eq!(
            started,
            ["/tmp/px-close-none/bin/pixel [\"doctor\"]".to_string()]
        );
    }

    #[test]
    fn close_should_return_without_asking_when_any_gate_holds() {
        // No notice, an owned exit code, a pipe on stdin, the updater
        // itself, and the opt-out each keep the question unasked.
        // No notice, an owned exit code, a mutating command, a pipe on
        // stdin, the updater itself, and the opt-out each keep the
        // question unasked.
        for (update_line, owned_exit, label, tty, read_only, env) in [
            (None, None, "search", true, true, env_of(&[])),
            (Some("pixel 9"), Some(3), "search", true, true, env_of(&[])),
            (Some("pixel 9"), None, "search", true, false, env_of(&[])),
            (Some("pixel 9"), None, "search", false, true, env_of(&[])),
            (
                Some("pixel 9"),
                None,
                "self-update",
                true,
                true,
                env_of(&[]),
            ),
            (
                Some("pixel 9"),
                None,
                "search",
                true,
                true,
                env_of(&[(PROMPT_OPT_OUT_VAR, "1")]),
            ),
        ] {
            let (asked, started) = closed(
                update_line,
                owned_exit,
                label,
                tty,
                read_only,
                env,
                vec![],
                true,
            );
            assert_eq!((asked, started), (0, vec![]), "{label} {tty:?}");
        }
    }

    #[test]
    fn close_should_stay_when_the_answer_declines_or_the_upgrade_fails() {
        let (asked, started) = closed(
            Some("pixel 9"),
            None,
            "search",
            true,
            true,
            env_of(&[]),
            vec![Ok("n\n")],
            true,
        );
        assert_eq!((asked, started), (1, vec![]));
        let (asked, started) = closed(
            Some("pixel 9"),
            None,
            "search",
            true,
            true,
            env_of(&[]),
            vec![Ok("y\n")],
            false,
        );
        assert_eq!((asked, started), (1, vec![]));
    }

    #[test]
    fn close_should_stay_when_the_binary_path_is_unknown() {
        let (mut ask, asked) = scripted(vec![Ok("y\n")]);
        let mut start = |_: &Path, _: &[String]| panic!("nothing to launch");
        close_with_update(
            Close {
                notice: Some("pixel 9"),
                owned_exit: None,
                command_label: "search",
                stdin_is_terminal: true,
                read_only: true,
                hint: Some("brew up".into()),
                exe: None,
                args: &[],
            },
            env_of(&[]),
            &mut ask,
            &mut |_| true,
            &mut start,
        );
        assert_eq!(asked(), 1);
    }
}
