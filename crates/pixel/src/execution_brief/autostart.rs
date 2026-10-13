// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Keeping a daemon behind the brief.
//!
//! The brief reads a warm daemon when one answers and the index in process
//! otherwise, and in process it has no `meaning` leads: the same prompts that
//! find the right file with the daemon find it far less often without. A
//! daemon exits after half an hour idle and a protocol bump leaves an older
//! one unusable, so after a break or an upgrade the first prompts would run
//! without it. This module starts one, in the background, from the two places
//! the brief already runs: a session starting (when it also asks the daemon
//! one question, so the `meaning` vectors are built by the time the first
//! prompt comes) and a prompt that found none (which keeps the in-process
//! route for itself and leaves the daemon to the next one).
//!
//! Nothing here waits for the daemon or fails a hook. The decision is a few
//! local socket probes; the start itself is one detached `pixel daemon start`
//! process that retires a stale daemon, waits for the new one and asks its
//! question, so none of that is the hook's time. Every call is bounded, and a
//! hook that gives up on a hanging probe has lost only a start.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crate::{DaemonProbe, config_cmd};

use super::chain::{BRIEF_ENV, Gate};

/// The longest a hook waits for a start to be decided and launched. A probe
/// of a wedged daemon is the only thing that comes near it.
pub(crate) const HOOK_BOUND: Duration = Duration::from_millis(100);

/// What allows the brief to start a daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Permit {
    /// The brief is switched on.
    pub(crate) brief: bool,
    /// The repository has a published index.
    pub(crate) indexed: bool,
    /// Auto-start is on, as for every other command.
    pub(crate) auto_start: bool,
}

impl Permit {
    /// The permit of `root`, read from the same switches the brief and the
    /// CLI's auto-start read.
    pub(crate) fn read(root: &Path) -> Self {
        Self::read_with(root, BRIEF_ENV, config_cmd::DAEMON_AUTO_START_ENV)
    }

    /// [`Permit::read`] with the names of the two environment switches given.
    pub(crate) fn read_with(root: &Path, brief_env: &str, auto_start_env: &str) -> Self {
        let gate = Gate::read_with(root, brief_env);
        Self {
            brief: gate.enabled,
            indexed: gate.indexed,
            auto_start: config_cmd::feature_enabled(
                Some(root),
                config_cmd::DAEMON_AUTO_START_FEATURE,
                auto_start_env,
            ),
        }
    }

    /// Why a start is not allowed, if it is not.
    fn refusal(self) -> Option<Skip> {
        if !self.brief {
            Some(Skip::BriefOff)
        } else if !self.indexed {
            Some(Skip::Unindexed)
        } else if !self.auto_start {
            Some(Skip::AutoStartOff)
        } else {
            None
        }
    }
}

/// Why nothing was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Skip {
    BriefOff,
    Unindexed,
    AutoStartOff,
    /// A newer pixel's daemon serves the repository: leave it to that pixel.
    NewerDaemon,
}

/// What a start attempt came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The start process was launched.
    Launched,
    /// A current daemon already answers and nothing needed launching.
    Running,
    Skipped(Skip),
    /// The start process could not be launched.
    LaunchFailed,
}

impl Outcome {
    /// The outcome as the decision log spells it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Launched => "launched",
            Self::Running => "running",
            Self::Skipped(Skip::BriefOff) => "skipped: brief off",
            Self::Skipped(Skip::Unindexed) => "skipped: no index",
            Self::Skipped(Skip::AutoStartOff) => "skipped: auto-start off",
            Self::Skipped(Skip::NewerDaemon) => "skipped: newer daemon",
            Self::LaunchFailed => "launch failed",
        }
    }
}

/// When a launch is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A prompt: only when no current daemon answers.
    Start,
    /// A session starting: also when one answers, because the start process
    /// asks it the question that wakes the `meaning` vectors.
    Warm,
}

/// The daemon of a repository, as the brief sees it. The production
/// implementation is the CLI's own probe, retirement and start.
pub(crate) trait Daemons: Send + Sync {
    /// What answers on the repository's socket.
    fn probe(&self, root: &Path) -> DaemonProbe;
    /// Tell a stale daemon to shut down, without waiting for it to.
    fn retire(&self, root: &Path);
    /// Launch the detached start process.
    fn launch(&self, root: &Path) -> io::Result<()>;
}

/// The CLI's own daemon handling.
pub(crate) struct Cli;

#[cfg_attr(test, mutants::skip)] // One-line adapters over the real socket and process; the decision above them is tested through `Daemons`.
impl Daemons for Cli {
    fn probe(&self, root: &Path) -> DaemonProbe {
        crate::probe_daemon(root)
    }

    fn retire(&self, root: &Path) {
        crate::retire_stale_daemon_within(root, Duration::ZERO);
    }

    fn launch(&self, root: &Path) -> io::Result<()> {
        let absolute = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        crate::spawn_detached(&crate::daemon_warm_args(&absolute))
    }
}

/// Decide and, if allowed, start. A stale daemon is told to go before the
/// start process is launched (which retires it again and waits, so the new
/// daemon never dies on the lock the old one holds).
pub(crate) fn ensure(root: &Path, mode: Mode, permit: Permit, daemons: &dyn Daemons) -> Outcome {
    if let Some(skip) = permit.refusal() {
        return Outcome::Skipped(skip);
    }
    match daemons.probe(root) {
        DaemonProbe::Newer => return Outcome::Skipped(Skip::NewerDaemon),
        DaemonProbe::Current if mode == Mode::Start => return Outcome::Running,
        DaemonProbe::Stale => daemons.retire(root),
        DaemonProbe::Current | DaemonProbe::Absent => {}
    }
    if daemons.launch(root).is_ok() {
        Outcome::Launched
    } else {
        Outcome::LaunchFailed
    }
}

/// A start running in the background.
pub(crate) struct Kicked {
    done: Receiver<Outcome>,
}

impl Kicked {
    /// The outcome, if it is known within `bound`; `None` when the probe or
    /// the launch is still going, which the caller does not wait for.
    pub(crate) fn wait(&self, bound: Duration) -> Option<Outcome> {
        self.done.recv_timeout(bound).ok()
    }
}

/// Run [`ensure`] on a thread of its own and return at once.
pub(crate) fn kick(root: &Path, mode: Mode, permit: Permit, daemons: Arc<dyn Daemons>) -> Kicked {
    let (send, done) = mpsc::channel();
    let root = root.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("pixel-autostart".into())
        .spawn(move || {
            let _ = send.send(ensure(&root, mode, permit, daemons.as_ref()));
        });
    // No thread to run on is a start not made, which `wait` reports as None.
    drop(spawned);
    Kicked { done }
}

/// Start (or warm) the daemon of `root` for a session that is starting, and
/// give the decision [`HOOK_BOUND`] at most. Never fails.
pub(crate) fn warm(root: &Path) -> Option<Outcome> {
    kick(root, Mode::Warm, Permit::read(root), Arc::new(Cli)).wait(HOOK_BOUND)
}

/// The kick a prompt gives the daemon, for the brief to hold until its hook
/// is about to exit.
pub(crate) fn start_for_prompt(root: &Path) -> Kicked {
    kick(root, Mode::Start, Permit::read(root), Arc::new(Cli))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    const ALLOWED: Permit = Permit {
        brief: true,
        indexed: true,
        auto_start: true,
    };

    /// A scripted daemon that records what was done to it, in order.
    struct Fake {
        probe: DaemonProbe,
        launch_fails: bool,
        /// How long the probe and the launch take.
        stall: Duration,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Fake {
        fn new(probe: DaemonProbe) -> Self {
            Self {
                probe,
                launch_fails: false,
                stall: Duration::ZERO,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn note(&self, call: &'static str) {
            self.calls.lock().unwrap().push(call);
            std::thread::sleep(self.stall);
        }
    }

    impl Daemons for Fake {
        fn probe(&self, _: &Path) -> DaemonProbe {
            self.note("probe");
            self.probe
        }

        fn retire(&self, _: &Path) {
            self.note("retire");
        }

        fn launch(&self, _: &Path) -> io::Result<()> {
            self.note("launch");
            if self.launch_fails {
                Err(io::ErrorKind::NotFound.into())
            } else {
                Ok(())
            }
        }
    }

    fn ensured(mode: Mode, permit: Permit, fake: &Fake) -> Outcome {
        ensure(Path::new("/repo"), mode, permit, fake)
    }

    #[test]
    fn an_absent_daemon_should_be_launched_on_a_prompt_and_on_a_session_start() {
        for mode in [Mode::Start, Mode::Warm] {
            let fake = Fake::new(DaemonProbe::Absent);
            assert_eq!(ensured(mode, ALLOWED, &fake), Outcome::Launched, "{mode:?}");
            assert_eq!(fake.calls(), ["probe", "launch"], "{mode:?}");
        }
    }

    #[test]
    fn a_current_daemon_should_be_left_alone_by_a_prompt_and_woken_by_a_session_start() {
        let fake = Fake::new(DaemonProbe::Current);
        assert_eq!(ensured(Mode::Start, ALLOWED, &fake), Outcome::Running);
        assert_eq!(fake.calls(), ["probe"], "a prompt launches nothing");
        let fake = Fake::new(DaemonProbe::Current);
        assert_eq!(ensured(Mode::Warm, ALLOWED, &fake), Outcome::Launched);
        assert_eq!(
            fake.calls(),
            ["probe", "launch"],
            "a session start asks the running daemon its question"
        );
    }

    #[test]
    fn a_stale_daemon_should_be_retired_before_the_launch() {
        for mode in [Mode::Start, Mode::Warm] {
            let fake = Fake::new(DaemonProbe::Stale);
            assert_eq!(ensured(mode, ALLOWED, &fake), Outcome::Launched, "{mode:?}");
            assert_eq!(fake.calls(), ["probe", "retire", "launch"], "{mode:?}");
        }
    }

    #[test]
    fn a_newer_daemon_should_be_left_to_the_newer_pixel() {
        for mode in [Mode::Start, Mode::Warm] {
            let fake = Fake::new(DaemonProbe::Newer);
            assert_eq!(
                ensured(mode, ALLOWED, &fake),
                Outcome::Skipped(Skip::NewerDaemon),
                "{mode:?}"
            );
            assert_eq!(fake.calls(), ["probe"], "{mode:?}");
        }
    }

    #[test]
    fn a_start_should_not_be_made_when_any_switch_is_off_and_nothing_should_be_touched() {
        let cases = [
            (
                Permit {
                    brief: false,
                    ..ALLOWED
                },
                Skip::BriefOff,
            ),
            (
                Permit {
                    indexed: false,
                    ..ALLOWED
                },
                Skip::Unindexed,
            ),
            (
                Permit {
                    auto_start: false,
                    ..ALLOWED
                },
                Skip::AutoStartOff,
            ),
        ];
        for (permit, skip) in cases {
            for probe in [
                DaemonProbe::Absent,
                DaemonProbe::Stale,
                DaemonProbe::Current,
            ] {
                for mode in [Mode::Start, Mode::Warm] {
                    let fake = Fake::new(probe);
                    assert_eq!(
                        ensured(mode, permit, &fake),
                        Outcome::Skipped(skip),
                        "{permit:?} {probe:?} {mode:?}"
                    );
                    assert!(fake.calls().is_empty(), "{permit:?} {probe:?} {mode:?}");
                }
            }
        }
        // The first reason found is the one reported.
        let none = Permit {
            brief: false,
            indexed: false,
            auto_start: false,
        };
        assert_eq!(
            ensured(Mode::Start, none, &Fake::new(DaemonProbe::Absent)),
            Outcome::Skipped(Skip::BriefOff)
        );
        let unindexed = Permit {
            indexed: false,
            auto_start: false,
            ..ALLOWED
        };
        assert_eq!(
            ensured(Mode::Start, unindexed, &Fake::new(DaemonProbe::Absent)),
            Outcome::Skipped(Skip::Unindexed)
        );
    }

    #[test]
    fn a_launch_that_fails_should_say_so_and_still_return() {
        let mut fake = Fake::new(DaemonProbe::Absent);
        fake.launch_fails = true;
        assert_eq!(ensured(Mode::Start, ALLOWED, &fake), Outcome::LaunchFailed);
        assert_eq!(fake.calls(), ["probe", "launch"]);
    }

    #[test]
    fn a_kicked_start_should_report_its_outcome() {
        let fake = Arc::new(Fake::new(DaemonProbe::Absent));
        let kicked = kick(Path::new("/repo"), Mode::Start, ALLOWED, fake.clone());
        assert_eq!(kicked.wait(Duration::from_secs(5)), Some(Outcome::Launched));
        assert_eq!(fake.calls(), ["probe", "launch"]);
    }

    #[test]
    fn a_hook_should_not_wait_longer_than_its_bound_for_a_hanging_probe_or_launch() {
        for hanging in [DaemonProbe::Absent, DaemonProbe::Stale] {
            let mut fake = Fake::new(hanging);
            fake.stall = Duration::from_secs(30);
            let fake = Arc::new(fake);
            let started = Instant::now();
            let kicked = kick(Path::new("/repo"), Mode::Warm, ALLOWED, fake);
            assert_eq!(kicked.wait(Duration::from_millis(50)), None, "{hanging:?}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{hanging:?}: the caller was held for {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn the_outcomes_should_be_spelled_for_the_decision_log() {
        let spelled = [
            (Outcome::Launched, "launched"),
            (Outcome::Running, "running"),
            (Outcome::Skipped(Skip::BriefOff), "skipped: brief off"),
            (Outcome::Skipped(Skip::Unindexed), "skipped: no index"),
            (
                Outcome::Skipped(Skip::AutoStartOff),
                "skipped: auto-start off",
            ),
            (Outcome::Skipped(Skip::NewerDaemon), "skipped: newer daemon"),
            (Outcome::LaunchFailed, "launch failed"),
        ];
        for (outcome, text) in spelled {
            assert_eq!(outcome.as_str(), text);
        }
    }

    /// A repository on disk with its own settings, and an index when asked.
    fn repo(tag: &str, settings: &str, indexed: bool) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pixel-autostart-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join(".pixel");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.yaml"), settings).unwrap();
        if indexed {
            let shard_dir = root.join(pixel_index::index::SHARD_DIR);
            std::fs::create_dir_all(&shard_dir).unwrap();
            std::fs::write(shard_dir.join(pixel_index::index::SHARD_FILE), b"x").unwrap();
        }
        root
    }

    #[test]
    fn the_permit_should_follow_the_settings_of_the_repository() {
        // Both keys are given in every file: the home settings are never
        // consulted when the repository answers.
        let cases = [
            (
                "on",
                "brief: true\ndaemon_auto_start: true\n",
                true,
                Permit {
                    brief: true,
                    indexed: true,
                    auto_start: true,
                },
            ),
            (
                "auto-off",
                "brief: true\ndaemon_auto_start: false\n",
                true,
                Permit {
                    brief: true,
                    indexed: true,
                    auto_start: false,
                },
            ),
            (
                "brief-off",
                "brief: false\ndaemon_auto_start: true\n",
                true,
                Permit {
                    brief: false,
                    indexed: true,
                    auto_start: true,
                },
            ),
            (
                "unindexed",
                "brief: true\ndaemon_auto_start: true\n",
                false,
                Permit {
                    brief: true,
                    indexed: false,
                    auto_start: true,
                },
            ),
        ];
        for (tag, settings, indexed, expected) in cases {
            let root = repo(tag, settings, indexed);
            // Names no test sets: the environment decides nothing here.
            let permit = Permit::read_with(
                &root,
                "PIXEL_TEST_AUTOSTART_UNSET_BRIEF",
                "PIXEL_TEST_AUTOSTART_UNSET_START",
            );
            assert_eq!(permit, expected, "{tag}");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn the_environment_should_decide_over_the_settings() {
        let root = repo("env", "brief: false\ndaemon_auto_start: false\n", true);
        let brief_env = format!("PIXEL_TEST_AUTOSTART_BRIEF_{}", std::process::id());
        let start_env = format!("PIXEL_TEST_AUTOSTART_START_{}", std::process::id());
        // SAFETY: both names carry this test's process id and line of origin
        // and nothing else reads or writes them.
        unsafe {
            std::env::set_var(&brief_env, "1");
            std::env::set_var(&start_env, "1");
        }
        let forced_on = Permit::read_with(&root, &brief_env, &start_env);
        // SAFETY: as above.
        unsafe {
            std::env::set_var(&brief_env, "off");
            std::env::set_var(&start_env, "0");
        }
        let forced_off = Permit::read_with(&root, &brief_env, &start_env);
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(&brief_env);
            std::env::remove_var(&start_env);
        }
        assert_eq!(
            forced_on,
            Permit {
                brief: true,
                indexed: true,
                auto_start: true
            }
        );
        assert_eq!(
            forced_off,
            Permit {
                brief: false,
                indexed: true,
                auto_start: false
            }
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
