// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The command line (`ARCHITECTURE.md`, "Command surface"): one subcommand
//! per row of that table, in this order. `docs_drift` holds the two together.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// The binary's one-line description in `--help`.
pub const ABOUT: &str = "Pixel speeds up coding agents by pushing deterministic evidence when it can remove a repeated investigation turn";

#[derive(Parser)]
#[command(name = "pixel", version, about = ABOUT, long_version = long_version())]
pub struct Cli {
    /// `off` silences the 🟩 lines for this invocation; the accounting stays.
    #[arg(long, global = true, default_value = "on", value_parser = ["on", "off"])]
    pub metrics: String,

    /// Answer in this process instead of asking the daemon.
    #[arg(long, global = true)]
    pub no_daemon: bool,

    #[command(subcommand)]
    pub command: Command,
}

fn long_version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        "\ncommit: ",
        env!("PIXEL_BUILD_COMMIT"),
        "\ntarget: ",
        env!("PIXEL_BUILD_TARGET"),
        "\nrustc: ",
        env!("PIXEL_RUSTC_VERSION"),
        "\nbuilt: ",
        env!("PIXEL_BUILD_DATE"),
    )
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DirectionArg {
    Upstream,
    Downstream,
}

impl DirectionArg {
    pub fn as_str(self) -> &'static str {
        match self {
            DirectionArg::Upstream => "upstream",
            DirectionArg::Downstream => "downstream",
        }
    }
}

/// The severity at which `review` exits non-zero.
#[derive(Clone, Copy, ValueEnum)]
pub enum FailOn {
    Blocker,
    Concern,
    Suggestion,
}

/// The status at which `doctor` exits non-zero.
#[derive(Clone, Copy, ValueEnum)]
pub enum DoctorFailOn {
    Yellow,
    Red,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build or refresh the text index and the code graph (and, with
    /// --history, the history db) of the repository containing PATH.
    Prepare {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also ingest commit metadata and diff text.
        #[arg(long)]
        history: bool,
        #[arg(long)]
        json: bool,
    },
    /// Report the freshness of the index, the graph and the history db.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Search the indexed tree with a regex, ripgrep-style: any number of
    /// paths (repo roots, subdirectories or files).
    SearchContent {
        pattern: String,
        /// Where to search; each is mapped to its repository root.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
        /// Keep or drop paths by glob; a leading `!` excludes.
        #[arg(short = 'g', long = "glob")]
        globs: Vec<String>,
        /// Keep a file type (rust, ts, py, md, ...).
        #[arg(short = 't', long = "type")]
        types: Vec<String>,
        /// Print only the paths of matching files.
        #[arg(short = 'l', long = "files-with-matches")]
        files_only: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Search the code by meaning: a question in your own words ranked over
    /// the code chunks (needs the `semantic` build and its model, see
    /// --download). Leads, never verdicts.
    SearchMeaning {
        query: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many files to lead with (at most 20).
        #[arg(long)]
        limit: Option<usize>,
        /// Fetch the embedding model into the cache, printing where from and
        /// where to first, then exit. The only command that downloads it.
        #[arg(long)]
        download: bool,
        #[arg(long)]
        json: bool,
    },
    /// Find where a phrase lives in the code: the string literals, UI text,
    /// routes, environment-variable names and config keys that carry its
    /// words, ranked, each with the symbol that owns the line.
    FindCode {
        phrase: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many leads to print (at most 100).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find a symbol by name or uid.
    FindSymbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// The callers of a symbol, or with --callees what it calls.
    WhoCalls {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Answer from every repository of `pixel workspace`, merged per repository.
        #[arg(long)]
        workspace: bool,
        /// List what the symbol calls instead of who calls it.
        #[arg(long)]
        callees: bool,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// The blast radius of a symbol, with the tests that cover it.
    Impact {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Answer from every repository of `pixel workspace`, merged per repository.
        #[arg(long)]
        workspace: bool,
        #[arg(long, value_enum, default_value = "upstream")]
        direction: DirectionArg,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Map the working tree (or a base) onto the symbols it changes.
    WhatChanged {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// A commit, branch or merge-base to compare with instead of HEAD.
        #[arg(long)]
        base: Option<String>,
        /// List the test files that exercise each changed symbol.
        #[arg(long)]
        tests: bool,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// A deterministic review of the working tree: readers a change left
    /// behind, untested changes, credential-shaped additions.
    Review {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        /// Exit non-zero when a finding is at least this severe.
        #[arg(long, value_enum)]
        fail_on: Option<FailOn>,
        #[arg(long)]
        json: bool,
    },
    /// The shortest chain of resolved calls from one symbol to another.
    CallPath {
        from: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 8)]
        depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// The declarations of one file: line, kind and signature.
    ListSignatures {
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// One symbol's source fitted to a token budget: its doc comment, its
    /// declaration and as much of the body as fits, with the lines left out
    /// counted.
    PackContext {
        /// A name or a uid.
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Tokens, at four bytes each.
        #[arg(long, default_value_t = 800)]
        budget: usize,
        #[arg(long)]
        json: bool,
    },
    /// Every indexed file with its symbols.
    RepoMap {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// The exportable document form.
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search commit messages, paths and diffs.
    SearchHistory {
        query: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// `message`, `path`, `diff` or `all`.
        #[arg(long)]
        facet: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Answer from the db as it stands: never ingest, never create it.
        #[arg(long)]
        read_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rename a symbol through the sites the graph resolved to it. A dry run
    /// unless --apply.
    Rename {
        name: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Write the edits.
        #[arg(long)]
        apply: bool,
        /// Also rename sites that are only probably the symbol.
        #[arg(long)]
        include_likely: bool,
        #[arg(long)]
        json: bool,
    },
    /// The brief a prompt would get: the evidence the hooks inject at prompt
    /// submit, computed by the daemon with no model. Prints nothing when the
    /// prompt gets no brief.
    Brief {
        /// The prompt, as typed.
        prompt: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// The host session the brief is for; ranges already sent to it are
        /// left out.
        #[arg(long)]
        session: Option<String>,
        /// The host the brief is delivered to.
        #[arg(long, default_value = "cli")]
        host: String,
        /// How many hook parts the host delivers it over.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        parts: u8,
        /// Recipe steps to leave out of this brief, to see what each is
        /// worth: workspace-overview, overview-answer-evidence,
        /// cand-k-member-manifest, cand-j-callers, cand-k-code-sites,
        /// cand-j-chain, cand-j-stop-cause, flow-rank, history-fallback, cand-k-commit-patch,
        /// error-origin, config-registry, test-bodies, doc-sections, name-files, rank-blocks,
        /// prune-noise.
        /// `PIXEL_BRIEF_STEPS=-name,+name` does the same for the daemon.
        #[arg(long, value_delimiter = ',', value_parser = parse_step)]
        without: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Answer one agent-host hook event (`specs/hosts.md`): read the host's
    /// payload on stdin, print one JSON envelope or nothing, and exit 0 whatever
    /// happens. The command every registered hook runs.
    Hook {
        /// What happened: session-start, prompt-submit, pre-edit, post-edit,
        /// post-tool or stop.
        event: pixel_setup::Event,
        /// The host that fired it: claude, codex, devin, gemini, antigravity,
        /// pi or opencode.
        #[arg(long)]
        host: pixel_setup::Host,
        /// Serve slice K of N of a brief split over several hooks.
        #[arg(long, value_name = "K/N")]
        part: Option<pixel_brief::parts::BriefPart>,
    },
    /// Register Pixel's hooks, plugin, package and skill with the agents on this
    /// machine, or take them back out. Idempotent; it preserves foreign hooks
    /// and settings, and uninstall removes only entries Pixel manages.
    ///
    /// ```text
    /// pixel setup --print        # what it would write, writing nothing
    /// pixel setup                # every configured agent
    /// pixel setup --agent claude,codex
    /// pixel setup --remove       # everything it wrote, and this binary
    /// ```
    Setup {
        /// Only these agents: claude, codex, devin, cursor, gemini,
        /// antigravity, opencode, pi, agents-md.
        #[arg(long, value_delimiter = ',', conflicts_with = "remove")]
        agent: Vec<pixel_setup::setup::AgentTarget>,
        #[arg(long)]
        json: bool,
        /// Report what would be written and write nothing.
        #[arg(long, conflicts_with = "remove")]
        print: bool,
        /// Do not ask which agents to set up: every agent found on the machine.
        #[arg(long, short = 'y', conflicts_with = "remove")]
        yes: bool,
        /// Remove everything `pixel setup` wrote: the skill, the hook entries,
        /// the pi package, OpenCode's plugin, Antigravity's plugin, and the
        /// pixel binary itself (left in place when a package manager owns it).
        #[arg(long)]
        remove: bool,
        /// With `--remove`: report what would be removed and change nothing.
        #[arg(long, requires = "remove")]
        dry_run: bool,
        /// With `--remove`: the binary to remove (default: the running one).
        #[arg(long, requires = "remove")]
        binary_path: Option<PathBuf>,
    },
    /// Check the install, the binary, the daemon and the freshness of the
    /// indexes, green/yellow/red per check, each flagged one with the command
    /// that repairs it.
    Doctor {
        /// The repository to check (default: the current one, if any).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Run each distinct repair once and say whether it held.
        #[arg(long)]
        fix: bool,
        /// Exit 1 when a check is at or above this status.
        #[arg(long, value_enum, default_value_t = DoctorFailOn::Red)]
        fail_on: DoctorFailOn,
        /// Run only these checks (ids, or `group.*`).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Leave these checks out (ids, or `group.*`).
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// The shell whose PATH `binary.shell-path` asks (default: the login shell).
        #[arg(long)]
        shell: Option<String>,
        /// List the checks and their repair commands.
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the settings in force and where each value comes from, or set one;
    /// `classify`, `classify-engine`, `remote-preset`, `remote-key` and `setup`
    /// configure the opt-in classifier.
    #[command(args_conflicts_with_subcommands = true)]
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
        /// The setting to show or change (default: list them all).
        key: Option<String>,
        /// The new value: `on`/`off`, or a number for `brief_parts`.
        value: Option<String>,
        /// Read or write the global `~/.pixel/config.yaml`, not the repository's.
        #[arg(long)]
        global: bool,
        /// Remove the setting from the file it would be written to.
        #[arg(long, requires = "key", conflicts_with = "value")]
        unset: bool,
        /// The repository (default: the current one, if any).
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// What the briefs, hooks and commands of this repository did: briefs
    /// delivered and declined, zero-call turns, injected versus read files,
    /// edit cards and Stop blocks, and the estimated savings of the commands.
    Metrics {
        /// How far back to look: `30m`, `24h`, `7d`, or `all`.
        #[arg(long, default_value = "24h")]
        since: String,
        /// The repository (default: the current one).
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Judge text against labels with an opt-in decision engine (off by
    /// default; nothing else in pixel calls a model).
    Classify(crate::classify::ClassifyOptions),
    /// The repositories that `impact --workspace` and `who-calls --workspace`
    /// answer across (`.pixel/workspace.json`).
    Workspace {
        #[command(subcommand)]
        cmd: crate::workspace_cmd::WorkspaceCmd,
    },
    /// Freeze this repository's index into one bundle, or install a bundle
    /// made elsewhere instead of rebuilding.
    Index {
        #[command(subcommand)]
        cmd: crate::index_cmd::IndexCmd,
    },
    /// Add a plugin: a directory, a git URL, or `github:<owner>/<repo>[@<ref>]`.
    Install {
        /// Where the plugin comes from.
        source: String,
        /// Install under this name instead of the manifest's.
        #[arg(long)]
        name: Option<String>,
        /// Install a release asset that publishes no `.sha256`.
        #[arg(long)]
        allow_unverified: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove an installed plugin and the skill it delivered.
    Uninstall { name: String },
    /// List, trust, enable or disable plugins.
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },
    /// Start, stop or check the repository's daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Guided global setup of the opt-in classifier in a terminal.
    Setup,
    /// Enable or disable all classify calls, including explicit --engine flags.
    Classify {
        #[arg(value_parser = ["on", "off"])]
        value: String,
    },
    /// API key for a `pixel classify` remote preset: `pixel config remote-key
    /// ollama -` reads it from stdin and stores it in the global
    /// configuration (never the repo config, never printed back). The
    /// provider env var still wins when set.
    RemoteKey {
        /// Remote provider preset the key belongs to.
        #[arg(value_enum)]
        preset: crate::decide_remote::Preset,
        /// The key value, or `-` to read it from stdin (keeps it out of
        /// shell history and `ps`); omit to report set/unset.
        value: Option<String>,
        /// Remove the stored key for this preset.
        #[arg(long)]
        clear: bool,
    },
    /// Remote provider `pixel classify` uses when the engine is `remote`:
    /// `pixel config remote-preset jev --model jev-latest`. A `--model` or
    /// `--base` left out clears the previous provider's stale choice.
    RemotePreset {
        /// Remote provider preset (openrouter, ollama, deepseek, opencode-go,
        /// openai, jev, clef-ollama, clef-cloudflare, local).
        #[arg(value_enum)]
        preset: crate::decide_remote::Preset,
        /// Model id the preset runs (default: the preset's own).
        #[arg(long)]
        model: Option<String>,
        /// Endpoint base override (default: the preset's own).
        #[arg(long)]
        base: Option<String>,
    },
    /// Which engine answers `pixel classify` when no `--engine` flag is given:
    /// `local` (an installed Ollaya server), `remote` (a hosted LLM behind a
    /// stored key), `jev`, `clef-ollama`, `clef-cloudflare` (remote engines on
    /// that preset), or `auto` (probe local, fall back to remote).
    ClassifyEngine {
        #[arg(value_parser = ["local", "remote", "jev", "clef-ollama", "clef-cloudflare", "auto"])]
        value: String,
    },
}

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List the plugins visible here: the repo's, your own, and `pixel-<name>` on PATH.
    List,
    /// Allow a plugin that declares `network = true` to run, or switch a brief probe back on.
    Enable { name: String },
    /// Switch a brief probe off: the prompt brief stops running it (its files stay).
    Disable { name: String },
    /// Allow a repo plugin (`<repo>/.pixel/plugins/<name>/`) as it is on disk now; any later change revokes it.
    Trust { name: String },
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Start the daemon (background unless --foreground).
    Start {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        foreground: bool,
    },
    /// Stop a running daemon.
    Stop {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check whether a daemon is running.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// A `--without` value: the name of a recipe step.
fn parse_step(name: &str) -> Result<String, String> {
    pixel_brief::steps::Step::parse(name).map(|step| step.name().to_owned())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_search_takes_ripgrep_style_flags_and_many_paths() {
        let cli = Cli::try_parse_from([
            "pixel",
            "search-content",
            "fn main",
            "src",
            "tests",
            "-g",
            "!*.md",
            "-t",
            "rust",
            "-l",
        ])
        .unwrap();
        let Command::SearchContent {
            pattern,
            paths,
            globs,
            types,
            files_only,
            ..
        } = cli.command
        else {
            panic!("a search");
        };
        assert_eq!(pattern, "fn main");
        assert_eq!(paths, [PathBuf::from("src"), PathBuf::from("tests")]);
        assert_eq!(
            (globs, types, files_only),
            (vec!["!*.md".to_owned()], vec!["rust".to_owned()], true)
        );
    }

    #[test]
    fn the_global_flags_come_before_or_after_the_subcommand() {
        for argv in [
            ["pixel", "--no-daemon", "status"],
            ["pixel", "status", "--no-daemon"],
        ] {
            assert!(Cli::try_parse_from(argv).unwrap().no_daemon);
        }
        assert!(Cli::try_parse_from(["pixel", "--metrics", "loud", "status"]).is_err());
    }
}
