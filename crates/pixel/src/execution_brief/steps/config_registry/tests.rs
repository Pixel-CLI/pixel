// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/config-registry/tests/test_probe.py` run on the step.

use std::os::unix::fs::symlink;
use std::path::Path;

use super::super::testing::{git, repo, run_kind};
use super::*;

const LOADER: &str = r#"import os
class Options:
    def __init__(self, defaults):
        self.data = defaults.copy()
    def load_env_file(self, env_name, silent=False):
        """doc-sentinel: deliberately excluded from executable evidence."""
        path = os.environ.get(env_name)
        if not path:
            if silent:
                return False
            raise ValueError("literal-sentinel")
        return self.load_file(path, silent=silent)
    def load_file(self, path, silent=False):
        values = parse_file(path)
        return self.apply(values)
    def apply(self, values):
        for key, value in values.items():
            self[key] = value
        diagnostic = f"fstring-sentinel {key}"
        password = 99887766  # comment-sentinel
"#;

const LOADER_PROMPT: &str = "Which environment-file overrides apply?";
const ETAG_PROMPT: &str = "where is the default etag setting defined and what is its value";
const WORKERS_PROMPT: &str = "how many workers does the program use and where can I override it";
const TRUST_PROMPT: &str = "Which code compiles the trust proxy setting, and how does req.ip use it?";

fn ask(files: &[(&str, &str)], prompt: &str) -> String {
    let dir = repo(files);
    run_kind(&STEP, dir.path(), prompt, "", Some("config"))
}

/// `files` written, only `tracked` of them staged.
fn ask_tracked(files: &[(&str, &str)], tracked: &[&str], prompt: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), files);
    git(dir.path(), &["init", "-q"]);
    let mut add = vec!["add", "--"];
    add.extend(tracked);
    git(dir.path(), &add);
    run_kind(&STEP, dir.path(), prompt, "", Some("config"))
}

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
}

fn has(out: &str, needles: &[&str]) {
    for needle in needles {
        assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
    }
}

fn lacks(out: &str, needles: &[&str]) {
    for needle in needles {
        assert!(!out.contains(needle), "unexpected {needle:?} in:\n{out}");
    }
}

#[test]
fn the_step_applies_to_a_config_brief_or_a_setting_prompt() {
    assert!(STEP.applies(Some("config"), "tour", false));
    assert!(STEP.applies(
        Some("lookup"),
        "where is the maximum zoom level defined",
        false
    ));
    assert!(STEP.applies(None, "which environment variables", false));
    assert!(!STEP.applies(Some("lookup"), "an unrelated prompt", false));
}

#[test]
fn beta_keeps_its_declaration_instead_of_inheriting_alpha_default() {
    let out = ask(
        &[
            (
                "a.js",
                "export const DEFAULT_TIMEOUT = 1101;\nexport function alpha() { return DEFAULT_TIMEOUT; }\n",
            ),
            (
                "b.js",
                "export const DEFAULT_TIMEOUT = 2202;\nexport function beta() { return DEFAULT_TIMEOUT; }\n",
            ),
        ],
        "Where is the DEFAULT_TIMEOUT setting defined and read in beta?",
    );
    has(
        &out,
        &[
            "DEFAULT_TIMEOUT @ b.js:1 | candidate value 2202",
            "DEFAULT_TIMEOUT @ a.js:1 | candidate value 1101",
            "2| export function beta()",
            "lexical binding and override precedence unknown",
        ],
    );
    lacks(&out, &["constant default 1101"]);
}

#[test]
fn same_file_scopes_keep_distinct_coordinates_without_inventing_bindings() {
    let out = ask(
        &[(
            "scope.js",
            "function alpha() {\n  const DEFAULT_TIMEOUT = 1101;\n}\nfunction beta() {\n  const DEFAULT_TIMEOUT = 2202;\n}\n",
        )],
        "Where is the DEFAULT_TIMEOUT setting in beta?",
    );
    has(
        &out,
        &[
            "DEFAULT_TIMEOUT @ scope.js:2 | candidate value 1101",
            "DEFAULT_TIMEOUT @ scope.js:5 | candidate value 2202",
            "4| function beta()",
            "lexical binding and override precedence unknown",
        ],
    );
}

#[test]
fn a_unique_constant_retains_its_source_value() {
    let out = ask(
        &[("only.js", "export const REQUEST_LIMIT = 37;\n")],
        "Where is the REQUEST_LIMIT setting declared?",
    );
    has(
        &out,
        &[
            "REQUEST_LIMIT | constant default 37 | only.js:1",
            "1| export const REQUEST_LIMIT = 37;",
        ],
    );
    lacks(&out, &["distinct declaration candidates"]);
}

#[test]
fn a_maximum_bound_question_matches_a_zoom_constant() {
    let out = ask(
        &[(
            "packages/common/src/constants.ts",
            "export const MIN_ZOOM = 0.1;\nexport const MAX_ZOOM = 30;\n",
        )],
        "where is the maximum zoom level defined and what is its value",
    );
    has(
        &out,
        &[
            "MAX_ZOOM | constant default 30 | packages/common/src/constants.ts:2",
            "2| export const MAX_ZOOM = 30;",
        ],
    );
    lacks(&out, &["MIN_ZOOM"]);
}

#[test]
fn a_maximum_pool_question_matches_the_pool_bound() {
    let out = ask(
        &[(
            "packages/common/src/constants.ts",
            "export const MAX_POOL_SIZE = 64;\nexport const MAX_IMAGE_SIZE = 8192;\n",
        )],
        "where is the maximum pool size defined and what is its value",
    );
    has(
        &out,
        &["MAX_POOL_SIZE | constant default 64 | packages/common/src/constants.ts:1"],
    );
    lacks(&out, &["MAX_IMAGE_SIZE"]);
}

#[test]
fn a_maximum_question_does_not_match_an_unrelated_bound() {
    let files = [(
        "packages/common/src/constants.ts",
        "export const REMOTE_MODEL = \"local\";\nexport const MAX_IMAGE_SIZE = 8192;\n// database cache tracks used size between measurements\nexport const EVICT_STEP = 25;\n",
    )];
    for prompt in [
        "where is the maximum remote database pool size defined",
        "where is the max remote database pool size defined",
        "where is the minimum remote database pool size defined",
        "where is the min remote database pool size defined",
    ] {
        let out = ask(&files, prompt);
        lacks(&out, &["REMOTE_MODEL", "MAX_IMAGE_SIZE", "EVICT_STEP"]);
    }
}

#[test]
fn existing_config_intent_keeps_matching_remote_setting() {
    let out = ask(
        &[(
            "packages/common/src/constants.ts",
            "export const REMOTE_MODEL = \"local\";\n",
        )],
        "where is the remote model setting configured",
    );
    has(&out, &["REMOTE_MODEL | constant default \"local\""]);
}

#[test]
fn literal_setting_calls_are_selected_by_generic_prompt_key() {
    let out = ask(
        &[
            ("src/app.js", "app.set('etag', 'weak');\n"),
            ("src/other.js", "app.set('trust proxy', true);\n"),
        ],
        ETAG_PROMPT,
    );
    has(
        &out,
        &[
            "app.set('etag', 'weak');",
            "### src/app.js:1-1 — literal setting call candidate",
        ],
    );
    lacks(&out, &["trust proxy"]);
}

#[test]
fn literal_setting_calls_require_tracked_non_symlink_source() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("src/app.js", "app.set('etag', 'weak');\n"),
            ("src/untracked.js", "app.set('etag', 'untracked-sentinel');\n"),
            ("outside/target.js", "app.set('etag', 'symlink-sentinel');\n"),
        ],
    );
    symlink(
        dir.path().join("outside/target.js"),
        dir.path().join("src/linked.js"),
    )
    .unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "--", "src/app.js", "src/linked.js"]);
    let out = run_kind(&STEP, dir.path(), ETAG_PROMPT, "", Some("config"));
    has(&out, &["app.set('etag', 'weak');"]);
    lacks(&out, &["untracked-sentinel", "symlink-sentinel"]);
}

#[test]
fn literal_setting_calls_keep_the_source_file_size_bound() {
    let large = format!(
        "app.set('etag', 'oversize-sentinel');\n{}",
        "x".repeat(400 * 1024)
    );
    let out = ask(
        &[
            ("src/app.js", "app.set('etag', 'weak');\n"),
            ("src/large.js", &large),
        ],
        ETAG_PROMPT,
    );
    has(&out, &["app.set('etag', 'weak');"]);
    lacks(&out, &["oversize-sentinel"]);
}

#[test]
fn literal_setting_source_masks_secret_values() {
    let out = ask(
        &[("src/app.js", "app.set(\"API_TOKEN\", \"setter-secret-sentinel\");\n")],
        "where is the API_TOKEN environment setting defined",
    );
    has(&out, &["literal setting call candidate"]);
    lacks(&out, &["setter-secret-sentinel"]);
}

#[test]
fn an_unrelated_intent_has_no_registry_output() {
    let files = [("only.js", "export const REQUEST_LIMIT = 37;\n")];
    assert_eq!(ask(&files, "Give me a tour of this repository"), "");
}

#[test]
fn env_candidates_do_not_establish_mode_precedence() {
    let out = ask(
        &[
            (".env.development", "APP_PORT=3101\n"),
            (".env.production", "APP_PORT=4101\n"),
            ("main.js", "const port = process.env.APP_PORT || 2101;\n"),
        ],
        "Which APP_PORT configuration applies in production?",
    );
    has(
        &out,
        &[
            "APP_PORT @ .env.development:1 | candidate value 3101",
            "APP_PORT @ .env.production:1 | candidate value 4101",
            "lexical binding and override precedence unknown",
            "process.env.APP_PORT || 2101",
        ],
    );
    lacks(&out, &["set in .env.development"]);
}

#[test]
fn untracked_env_templates_are_not_evidence() {
    let out = ask_tracked(
        &[
            ("main.js", "const port = process.env.APP_PORT || 2101;\n"),
            (".env.example", "APP_PORT=5101\n"),
            (".env.development", "APP_PORT=6101\n"),
        ],
        &["main.js"],
        "What is the APP_PORT environment variable default?",
    );
    has(&out, &["2101"]);
    lacks(&out, &["5101", "6101", "### .env."]);
}

#[test]
fn a_tracked_env_template_keeps_its_nonsecret_value() {
    let out = ask(
        &[
            (".env.example", "APP_PORT=3101\n"),
            ("main.js", "const port = process.env.APP_PORT || 2101;\n"),
        ],
        "What is the APP_PORT environment variable default?",
    );
    has(
        &out,
        &[
            "set in .env.example:1 = 3101",
            "code fallback 2101",
            "1| APP_PORT=3101",
        ],
    );
}

#[test]
fn env_lane_retains_the_general_scan_line_budget() {
    let long = format!("SERVICE_ORIGIN={}\n", "x".repeat(301));
    let out = ask(
        &[
            (".env.example", &long),
            ("main.js", "const origin = process.env.SERVICE_ORIGIN;\n"),
        ],
        "Where is the SERVICE_ORIGIN environment variable declared?",
    );
    lacks(&out, &["### .env.example:", &"x".repeat(100)]);
}

#[test]
fn env_line_budget_counts_utf8_bytes_like_ripgrep() {
    let long = format!("SERVICE_ORIGIN={}\n", "é".repeat(150));
    let out = ask(
        &[
            (".env.example", &long),
            ("main.js", "const origin = process.env.SERVICE_ORIGIN;\n"),
        ],
        "Where is the SERVICE_ORIGIN environment variable declared?",
    );
    lacks(&out, &["### .env.example:", &"é".repeat(100)]);
}

#[test]
fn a_tracked_env_symlink_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("main.js", "const port = process.env.APP_PORT || 2101;\n"),
            ("outside/env", "APP_PORT=7101\n"),
        ],
    );
    symlink(
        dir.path().join("outside/env"),
        dir.path().join(".env.development"),
    )
    .unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "--", "main.js", ".env.development"]);
    let out = run_kind(
        &STEP,
        dir.path(),
        "What is the APP_PORT environment variable default?",
        "",
        Some("config"),
    );
    lacks(&out, &["7101", "### .env.development:"]);
}

#[test]
fn secrets_are_masked_in_candidates_fallbacks_and_source_context() {
    let out = ask(
        &[
            (".env.development", "SERVICE_TOKEN=env-sentinel\n"),
            (
                "main.js",
                "const session = process.env.SERVICE_TOKEN || \"fallback-sentinel\";\n",
            ),
        ],
        "Where is the SERVICE_TOKEN environment variable default?",
    );
    has(&out, &["SERVICE_TOKEN", "<masked>"]);
    lacks(&out, &["env-sentinel", "fallback-sentinel"]);
}

#[test]
fn candidate_context_masks_a_neighbouring_secret() {
    let out = ask(
        &[
            (
                "a.js",
                "export const REQUEST_LIMIT = 37;\nconst API_KEY = \"neighbor-sentinel\";\n",
            ),
            ("b.js", "export const REQUEST_LIMIT = 42;\n"),
        ],
        "Where is the REQUEST_LIMIT setting declared?",
    );
    has(&out, &["REQUEST_LIMIT @ a.js:1", "REQUEST_LIMIT @ b.js:1"]);
    lacks(&out, &["neighbor-sentinel"]);
}

#[test]
fn bounded_candidates_report_omissions_instead_of_claiming_a_unique_default() {
    let texts: Vec<(String, String)> = (0..6)
        .map(|i| {
            (
                format!("{i}.js"),
                format!("export const REQUEST_LIMIT = {};\n", 30 + i),
            )
        })
        .collect();
    let files: Vec<(&str, &str)> = texts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let out = ask(&files, "Where is the REQUEST_LIMIT setting declared?");
    assert_eq!(out.matches("REQUEST_LIMIT @").count(), 4, "{out}");
    has(
        &out,
        &["2 additional candidates omitted; no unique default established"],
    );
}

#[test]
fn env_named_file_loader_keeps_failure_mode_and_overwrite_chain_together() {
    let out = ask(&[("loader.py", LOADER)], LOADER_PROMPT);
    has(
        &out,
        &[
            "Candidate Options.load_env_file @ loader.py:5",
            "self.data = defaults.copy()",
            "path = os.environ.get(env_name)",
            "if silent:",
            "return False",
            "self.load_file(path, silent=silent)",
            "self.apply(values)",
            "self[key] = value",
            "active mode, file contents and effective values unknown",
        ],
    );
    lacks(&out, &["doc-sentinel", "### loader.py:5-7"]);
}

#[test]
fn loader_candidates_do_not_collapse_homonymous_methods_across_classes() {
    let source = format!(
        "{LOADER}{}",
        LOADER.replace("import os\n", "").replace("Options", "OtherOptions")
    );
    let out = ask(&[("loader.py", &source)], LOADER_PROMPT);
    has(
        &out,
        &[
            "Candidate Options.load_env_file @ loader.py:5",
            "Candidate OtherOptions.load_env_file",
        ],
    );
    assert_eq!(out.matches("self[key] = value").count(), 2, "{out}");
}

fn flags(source: &str, prompt: &str) -> String {
    ask(&[("flags.rs", source)], prompt)
}

#[test]
fn cli_setting_follows_option_name_to_its_function_scoped_runtime_value() {
    let source = "
trait Flag {}
struct WorkerCount;
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { \"workers\" }
    fn doc_short(&self) -> &'static str { \"Set worker count.\" }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    let workers = if let Some(workers) = low.workers {
        workers
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    workers
}
";
    let out = flags(source, WORKERS_PROMPT);
    has(
        &out,
        &[
            "matched setting consumed within one function",
            "available_parallelism()",
            "low.workers",
            "matching CLI setting declaration and override",
            "fn name_long",
            "args.workers",
        ],
    );
    assert!(
        Regex::new(r"### flags\.rs:\d+-\d+ — matching CLI setting declaration and override")
            .unwrap()
            .is_match(&out),
        "{out}"
    );
}

#[test]
fn cli_setting_does_not_join_an_unrelated_parallelism_function() {
    let source = "
trait Flag {}
struct WorkerCount;
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { \"workers\" }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn configured(low: LowArgs) -> Option<usize> { low.workers }
fn unrelated_capacity() -> usize { std::thread::available_parallelism().map_or(1, |n| n.get()) }
";
    assert_eq!(flags(source, WORKERS_PROMPT), "");
}

#[test]
fn cli_setting_selector_matches_a_different_option_name() {
    let source = "
trait Flag {}
struct PoolSize;
impl Flag for PoolSize {
    fn name_long(&self) -> &'static str { \"database-pool\" }
    fn update(&self, args: &mut LowArgs) { args.pool_size = Some(12); }
}
fn resolve(low: LowArgs) -> usize {
    let pool_size = low.pool_size.unwrap_or(12);
    pool_size
}
";
    let out = flags(source, "where can I override the database pool option");
    has(
        &out,
        &[
            "database-pool",
            "args.pool_size",
            "low.pool_size",
            "matched setting consumed within one function",
        ],
    );
}

#[test]
fn cli_setting_selector_ignores_test_only_flag_implementations() {
    let source = "
trait Flag {}
struct WorkerCount;
#[cfg(test)]
impl Flag for WorkerCount {
    fn name_long(&self) -> &'static str { \"workers\" }
    fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    low.workers.unwrap_or(8)
}
";
    assert_eq!(flags(source, WORKERS_PROMPT), "");
}

#[test]
fn cli_setting_selector_ignores_flag_implementations_in_test_modules() {
    let source = "
trait Flag {}
struct WorkerCount;
#[cfg(test)]
mod tests {
    use super::*;
    impl Flag for WorkerCount {
        fn name_long(&self) -> &'static str { \"workers\" }
        fn update(&self, args: &mut LowArgs) { args.workers = Some(8); }
    }
}
fn resolve(low: LowArgs) -> usize {
    low.workers.unwrap_or(8)
}
";
    assert_eq!(flags(source, WORKERS_PROMPT), "");
}

#[test]
fn cli_setting_resolves_an_explicit_short_option_to_its_runtime_value() {
    let source = "
trait Flag {}
struct LineBuffered;
impl Flag for LineBuffered {
    fn name_long(&self) -> &'static str { \"line-buffered\" }
    fn update(&self, args: &mut LowArgs) { args.buffer = BufferMode::Line; }
}
struct Threads;
impl Flag for Threads {
    fn name_short(&self) -> Option<u8> { Some(b'j') }
    fn name_long(&self) -> &'static str { \"threads\" }
    fn doc_short(&self) -> &'static str { \"Set the approximate number of threads.\" }
    fn update(&self, args: &mut LowArgs) { args.threads = Some(8); }
}
fn resolve(low: LowArgs) -> usize {
    if let Some(threads) = low.threads { threads }
    else { std::thread::available_parallelism().map_or(1, |n| n.get()).min(12) }
}
";
    let out = flags(
        source,
        "When I omit -j, how is the worker count calculated, and what command-line flag lets me set it?",
    );
    has(
        &out,
        &[
            "Some(b'j')",
            "name_long",
            "available_parallelism()",
            "low.threads",
        ],
    );
    lacks(&out, &["LineBuffered"]);
}

#[test]
fn cli_setting_short_option_does_not_match_a_different_flag() {
    let source = "
trait Flag {}
struct Threads;
impl Flag for Threads {
    fn name_short(&self) -> Option<u8> { Some(b'k') }
    fn name_long(&self) -> &'static str { \"threads\" }
    fn update(&self, args: &mut LowArgs) { args.threads = Some(8); }
}
fn resolve(low: LowArgs) -> usize { low.threads.unwrap_or(8) }
";
    assert_eq!(
        flags(source, "When I omit -j, how is the worker count calculated?"),
        ""
    );
}

#[test]
fn cli_setting_short_option_does_not_parse_double_dash_as_single_short() {
    let source = "
trait Flag {}
struct Threads;
impl Flag for Threads {
    fn name_short(&self) -> Option<u8> { Some(b'j') }
    fn name_long(&self) -> &'static str { \"threads\" }
    fn update(&self, args: &mut LowArgs) { args.threads = Some(8); }
}
fn resolve(low: LowArgs) -> usize { low.threads.unwrap_or(8) }
";
    assert_eq!(
        flags(
            source,
            "When I omit --j, how is the worker count calculated, and what command-line flag lets me set it?"
        ),
        ""
    );
}

const APPLICATION: &str = "
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  this.settings[setting] = val;
  switch (setting) {
    case 'trust proxy':
      this.set('trust proxy fn', compileTrust(val));
      break;
  }
};
";

#[test]
fn compiled_setting_connects_the_named_compiler_and_exact_getter_consumer() {
    let out = ask(
        &[
            ("lib/application.js", APPLICATION),
            (
                "lib/utils.js",
                "
exports.compileTrust = function(val) {
  const API_TOKEN = \"compiler-secret-sentinel\";
  if (typeof val === 'function') return val;
  if (typeof val === 'number') return function(a, i){ return i < val };
  return proxyaddr.compile(val || []);
};
",
            ),
            (
                "lib/request.js",
                "
defineGetter(req, 'ip', function ip(){
  var trust = this.app.get('trust proxy fn');
  return proxyaddr(this, trust);
});
",
            ),
        ],
        TRUST_PROMPT,
    );
    has(
        &out,
        &[
            "case 'trust proxy'",
            "compileTrust(val)",
            "exports.compileTrust",
            "defineGetter(req, 'ip'",
            "get('trust proxy fn')",
            "proxyaddr(this, trust)",
        ],
    );
    lacks(&out, &["compiler-secret-sentinel"]);
}

#[test]
fn compiled_setting_does_not_join_a_different_getter_or_comment_decoy() {
    let out = ask(
        &[
            (
                "lib/application.js",
                "
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  switch (setting) { case 'trust proxy': this.set('trust proxy fn', compileTrust(val)); }
};
// case 'trust proxy': this.set('trust proxy fn', fakeCompile(val));
",
            ),
            (
                "lib/utils.js",
                "exports.compileTrust = function(val) { return proxyaddr.compile(val || []); };\n",
            ),
            (
                "lib/request.js",
                "
defineGetter(req, 'ip', function ip(){ return proxyaddr(this, this.app.get('session proxy fn')); });
defineGetter(req, 'secure', function secure(){ return this.app.get('trust proxy fn'); });
",
            ),
        ],
        TRUST_PROMPT,
    );
    lacks(
        &out,
        &[
            "named getter reads the matching compiled setting",
            "exact imported setting compiler implementation",
            "proxyaddr(this, trust)",
        ],
    );
}

#[test]
fn compiled_setting_ignores_comment_only_compile_branches() {
    let out = ask(
        &[
            (
                "lib/application.js",
                "
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  switch (setting) { case 'other proxy': this.set('other proxy fn', compileTrust(val)); }
};
// case 'trust proxy': this.set('trust proxy fn', compileTrust(val));
",
            ),
            (
                "lib/utils.js",
                "exports.compileTrust = function(val) { return proxyaddr.compile(val || []); };\n",
            ),
            (
                "lib/request.js",
                "
defineGetter(req, 'ip', function ip(){ return proxyaddr(this, this.app.get('trust proxy fn')); });
",
            ),
        ],
        TRUST_PROMPT,
    );
    lacks(
        &out,
        &[
            "setting branch installs a compiled function",
            "named getter reads the matching compiled setting",
        ],
    );
}

#[test]
fn compiled_setting_ignores_multiline_template_decoys() {
    let out = ask(
        &[
            (
                "lib/application.js",
                "
const docs = `
var compileTrust = require('./utils').compileTrust;
app.set('trust proxy', false);
app.set = function set(setting, val) {
  switch (setting) {
    case 'trust proxy': this.set('trust proxy fn', compileTrust(val)); break;
  }
};
`;
",
            ),
            (
                "lib/utils.js",
                "exports.compileTrust = function(val) { return proxyaddr.compile(val || []); };\n",
            ),
            (
                "lib/request.js",
                "
const docs = `
defineGetter(req, 'ip', function ip(){
  var trust = this.app.get('trust proxy fn');
  return proxyaddr(this, trust);
});
`;
",
            ),
        ],
        TRUST_PROMPT,
    );
    lacks(
        &out,
        &[
            "setting branch installs a compiled function",
            "exact imported setting compiler implementation",
            "named getter reads the matching compiled setting",
        ],
    );
}

const KEYS: &str = "export const PREFERENCE_KEYS = {\n  COLOR_THEME: \"draw-color-scheme\",\n  ACCOUNT_TOKEN: \"account-secret\",\n} as const;\n";
const THEME_PROMPT: &str = "how does the color theme setting persist between sessions";

#[test]
fn persistent_browser_preference_pairs_exact_subject_key_and_resolves_import() {
    let out = ask(
        &[
            (
                "src/preferences.ts",
                "import { PREFERENCE_KEYS } from \"./keys\";
export function useTheme() {
  const theme = localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME) || \"light\";
  localStorage.setItem(PREFERENCE_KEYS.COLOR_THEME, theme);
  return theme;
}
export function useAccount() {
  const token = localStorage.getItem(PREFERENCE_KEYS.ACCOUNT_TOKEN);
  localStorage.setItem(PREFERENCE_KEYS.ACCOUNT_TOKEN, token || \"\");
  return token;
}
",
            ),
            ("src/keys.ts", KEYS),
        ],
        THEME_PROMPT,
    );
    has(
        &out,
        &[
            "browser preference read/write using the same subject-matched storage key",
            "localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME)",
            "localStorage.setItem(PREFERENCE_KEYS.COLOR_THEME, theme)",
            "COLOR_THEME: \"draw-color-scheme\"",
        ],
    );
    lacks(&out, &["ACCOUNT_TOKEN"]);
}

#[test]
fn persistent_browser_preference_rejects_unpaired_and_unrelated_storage_keys() {
    let out = ask(
        &[
            (
                "src/preferences.ts",
                "import { PREFERENCE_KEYS } from \"./keys\";
export function useTheme() {
  return localStorage.getItem(PREFERENCE_KEYS.COLOR_THEME);
}
export function useAccount() {
  localStorage.setItem(PREFERENCE_KEYS.ACCOUNT_TOKEN, \"value\");
}
",
            ),
            ("src/keys.ts", KEYS),
        ],
        THEME_PROMPT,
    );
    lacks(
        &out,
        &[
            "browser preference read/write using the same subject-matched storage key",
            "localStorage.getItem",
            "localStorage.setItem",
        ],
    );
}

#[test]
fn an_unconnected_read_or_absent_writer_is_not_a_loader_chain() {
    for source in [
        LOADER.replace("self.load_file(path,", "self.load_file(other_path,"),
        LOADER.replace("self[key] = value", "print(key)"),
    ] {
        let out = ask(&[("loader.py", &source)], LOADER_PROMPT);
        lacks(&out, &["configuration loader"]);
    }
}

#[test]
fn loader_literals_fstrings_numbers_comments_and_docstrings_are_not_values() {
    let out = ask(&[("loader.py", LOADER)], LOADER_PROMPT);
    has(&out, &["self[key] = value", "<masked>"]);
    lacks(
        &out,
        &[
            "literal-sentinel",
            "fstring-sentinel",
            "99887766",
            "comment-sentinel",
            "doc-sentinel",
        ],
    );
}

#[test]
fn loader_source_requires_tracking_and_does_not_follow_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[("loader.py", LOADER), ("keep.js", "const OTHER_LIMIT = 10;\n")],
    );
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "--", "keep.js"]);
    let ask_here = || run_kind(&STEP, dir.path(), LOADER_PROMPT, "", Some("config"));
    lacks(&ask_here(), &["configuration loader"]);
    symlink(dir.path().join("loader.py"), dir.path().join("link.py")).unwrap();
    git(dir.path(), &["add", "--", "link.py"]);
    lacks(&ask_here(), &["configuration loader"]);
}

#[test]
fn loader_evidence_stays_absent_for_unrelated_intent() {
    assert_eq!(
        ask(&[("loader.py", LOADER)], "Give me a tour of this repository"),
        ""
    );
}

#[test]
fn loader_candidate_cap_reports_unresolved_omissions() {
    let source = format!(
        "import os\n{}",
        (0..3)
            .map(|i| LOADER
                .replace("import os\n", "")
                .replace("Options", &format!("Options{i}")))
            .collect::<String>()
    );
    let out = ask(&[("loader.py", &source)], LOADER_PROMPT);
    assert_eq!(out.matches("Candidate Options").count(), 2, "{out}");
    has(
        &out,
        &["Additional loader candidates omitted: 1; no unique loader established."],
    );
}
