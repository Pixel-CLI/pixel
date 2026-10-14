#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Class and information NEED of one agent tool call (issue #905).

``classify(tool, input)`` returns ``(class, need)``:

* ``class`` is what the call is: read, search, list, edit, write, build, test,
  run, git-read, git-write, web, pixel, other.
* ``need`` is what the agent wanted to know or do: find-definition,
  find-usages, read-region, read-whole-file, read-config, read-docs,
  search-string, search-regex, list-files, list-tree, git-history, git-status,
  run-tests, build-typecheck, run-program, web-lookup, edit-code, other. ``other`` is
  a recognised call that is not an information need (a git commit, a todo
  update, a sub-agent spawn); ``unknown`` is a call no rule recognises.

The rules read the tool name and its parsed arguments, never the transcript
prose. A Bash command line is split into pipelines with the shell tokenizer of
``scripts/excavation-count.py`` (imported from it: ``split_pipelines``,
``strip_wrappers`` handle heredocs, ``&&``/``;`` chains, ``env``/``timeout``
wrappers and the ``rtk`` proxy) and each pipeline is classified on its first
stage; the verdict of the highest-priority pipeline wins.

Standard library only; ``python3 -I scripts/tool-census/test_classify.py``
runs the rule tests.
"""

import importlib.util
import json
import os
import re

CLASSES = ("read", "search", "list", "edit", "write", "build", "test", "run",
           "git-read", "git-write", "web", "pixel", "other")
NEEDS = ("find-definition", "find-usages", "read-region", "read-whole-file", "read-config",
         "read-docs", "search-string", "search-regex", "list-files", "list-tree",
         "git-history", "git-status", "run-tests", "build-typecheck", "run-program",
         "web-lookup", "edit-code", "other", "unknown")


def _load_excavation():
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "excavation-count.py")
    spec = importlib.util.spec_from_file_location("excavation_count", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_EXC = _load_excavation()
split_pipelines = _EXC.split_pipelines
strip_wrappers = _EXC.strip_wrappers
SHELLS = _EXC.SHELLS
SHELL_C = _EXC.SHELL_C
PIXEL_BINARIES = _EXC.PIXEL_BINARIES

# Which pipeline decides when a command line chains several (``cd x && cargo test``).
PRIORITY = ("edit", "write", "test", "build", "git-write", "pixel", "web", "search", "read",
            "git-read", "list", "run", "other")

# --------------------------------------------------------------------------
# Paths
# --------------------------------------------------------------------------

CONFIG_NAMES = {
    "package.json", "tsconfig.json", "jsconfig.json", "cargo.toml", "pyproject.toml", "setup.py",
    "setup.cfg", "pipfile", "makefile", "justfile", "dockerfile", "docker-compose.yml",
    "docker-compose.yaml", "compose.yml", "compose.yaml", "go.mod", "gemfile", "build.gradle",
    "pom.xml", "package.swift", "mise.toml", "rust-toolchain", "rust-toolchain.toml", "deny.toml",
    "clippy.toml", "rustfmt.toml", "settings.json", "settings.local.json", "launch.json",
    "composer.json", "vercel.json", "netlify.toml", "wrangler.toml", "bunfig.toml", ".gitignore",
    ".gitattributes", ".editorconfig", ".npmrc", ".nvmrc", "marketplace.json", "plugin.json",
    "manifest.json", "hooks.json", "config.json", "config.yaml", "config.yml", "config.toml",
}
CONFIG_SUFFIXES = (".toml", ".ini", ".cfg", ".conf", ".plist", ".yaml", ".yml", ".env", ".lock")
CONFIG_PARTS = (".github/workflows/", ".config/", "/.claude/", "/.codex/", "/.pi/")
CONFIG_PATTERN = re.compile(
    r"(^|/)(\.env(\.[\w.-]+)?|\.[\w-]+rc(\.(json|js|cjs|mjs|ya?ml))?|"
    r"(vite|next|webpack|jest|vitest|babel|tailwind|eslint|prettier|rollup|astro|nuxt|svelte|"
    r"playwright|cypress|postcss|biome|turbo|drizzle|hugo)\.config\.[\w]+|"
    r"requirements[\w.-]*\.txt|tsconfig[\w.-]*\.json|bun\.lockb?|[\w-]+\.config\.(json|toml))$",
    re.IGNORECASE)
DOC_SUFFIXES = (".md", ".mdx", ".rst", ".adoc", ".markdown")
DOC_NAMES = {"readme", "changelog", "contributing", "license", "authors", "notice", "security",
             "architecture", "claude.md", "agents.md"}
DOC_PARTS = ("/docs/", "/doc/", "docs/", "/documentation/")


def _norm_path(path):
    return str(path or "").replace("\\", "/").strip()


def is_config_path(path):
    path = _norm_path(path)
    lowered = path.lower()
    base = os.path.basename(lowered)
    if base in CONFIG_NAMES or lowered.endswith(CONFIG_SUFFIXES) or CONFIG_PATTERN.search(lowered):
        return not lowered.endswith(DOC_SUFFIXES)
    return any(part in lowered for part in CONFIG_PARTS) and not lowered.endswith(DOC_SUFFIXES)


def is_doc_path(path):
    lowered = _norm_path(path).lower()
    base = os.path.basename(lowered)
    stem = base.split(".")[0]
    if lowered.endswith(DOC_SUFFIXES) or base in DOC_NAMES or stem in DOC_NAMES:
        return True
    return any(part in lowered for part in DOC_PARTS) and lowered.endswith((".txt", ".html", ".rst"))


def read_need(paths, region=False):
    """read-config / read-docs by what is read, else read-region / read-whole-file."""
    paths = [p for p in paths if p]
    if paths and all(is_config_path(p) for p in paths):
        return "read-config"
    if paths and all(is_doc_path(p) for p in paths):
        return "read-docs"
    return "read-region" if region else "read-whole-file"


# --------------------------------------------------------------------------
# Search patterns
# --------------------------------------------------------------------------

DEF_KEYWORD = (r"(?:pub(?:\\?\([\w:\s]+\\?\))?\s+|export\s+|default\s+|async\s+|static\s+|public\s+|"
               r"private\s+|protected\s+|abstract\s+|final\s+|unsafe\s+|const\s+|extern\s+)*")
DEF_WORDS = r"(?:fn|def|class|func|function|struct|enum|trait|impl|interface|type|mod|macro_rules!|protocol|extension|object|record|module|data|newtype|message|service|actor|const|let|var|val)"
# A definition pattern: ``fn name``, ``def name``, ``class Name``, ``(fn|def) name`` and its escaped forms.
DEF_RE = re.compile(
    r"(?:^|[\^|(\s\"'])\s*" + DEF_KEYWORD + r"\(?\??:?" + DEF_WORDS + r"(?:\|[a-z_!]+)*\)?\??(?:\\s[+*]?|\s|\[ \]|\[\\s\])+"
    r"[\w$\\(]", re.IGNORECASE)
DEF_ASSIGN_RE = re.compile(r"\b(?:const|let|var|val)\s+\w+\s*=|\bfunction\s*\*?\s*\w+|\bdef\s+\w+|\bfn\s+\w+")
IDENT_RE = re.compile(r"^(?:\\b|\\<|\\W)?(?:[.:#@$]|::|->)?[A-Za-z_$][\w$]*(?:\\b|\\>)?(?:\\?\(|\()?$")
CODEISH_RE = re.compile(r"[_$]|[a-z][A-Z]|[A-Z][a-z]+[A-Z]|^[A-Z][a-z][A-Za-z0-9]{2,}$|::")
REGEX_META_RE = re.compile(r"\.\*|\.\+|\\[dswDSWbB.]|\[[^\]]+\]|\(\?|[\^$]|\{\d|\*\$|\+\?|\\\||\|")


def _alternatives(pattern):
    return [a for a in re.split(r"(?<!\\)\|", pattern) if a] or [pattern]


def search_need(pattern, fixed=False, regex_flag=False):
    """Need of one search pattern: find-definition, find-usages, search-string or search-regex."""
    pattern = (pattern or "").strip()
    if not pattern:
        return "search-string"
    if not fixed and (DEF_RE.search(pattern) or DEF_ASSIGN_RE.search(pattern)):
        return "find-definition"
    if fixed and re.match(r"^(?:pub\s+)?(?:async\s+)?(?:fn|def|class|func|function|struct|enum|trait|interface|type)\s+\w+", pattern):
        return "find-definition"
    parts = _alternatives(pattern.replace("\\|", "|")) if not fixed else [pattern]
    if all(IDENT_RE.match(p.replace("\\|", "")) for p in parts):
        stripped = [re.sub(r"^\\[bBW<]|\\[b>]$", "", p) for p in parts]
        if any(CODEISH_RE.search(s) for s in stripped) or len(parts) > 1:
            return "find-usages"
    if re.match(r"^[\w$.:]+\(?$", pattern) and CODEISH_RE.search(pattern) and not re.search(r"\s", pattern):
        return "find-usages"
    if fixed:
        return "search-string"
    if regex_flag or REGEX_META_RE.search(pattern):
        return "search-regex"
    return "search-string"


# --------------------------------------------------------------------------
# Bash stages
# --------------------------------------------------------------------------

GREP_VALUE_FLAGS = {"-A", "-B", "-C", "-m", "-f", "-g", "-t", "-T", "-j", "-d", "-D", "-M", "-r0",
                    "--max-count", "--glob", "--type", "--type-not", "--include", "--exclude",
                    "--exclude-dir", "--include-dir", "--max-depth", "--after-context",
                    "--before-context", "--context", "--file", "--threads", "--iglob", "--max-columns",
                    "--sort", "--sortr", "--replace", "--colors", "--engine", "--pre", "--path-separator"}
GREP_BASES = {"grep", "egrep", "fgrep", "rg", "ag", "ack", "ugrep", "ripgrep", "pcregrep", "zgrep"}
READ_BASES = {"cat", "bat", "less", "more", "nl", "head", "tail", "od", "xxd", "strings", "hexdump",
              "batcat", "view", "zcat", "pr", "tac", "rev"}
LIST_BASES = {"ls", "tree", "exa", "eza", "lsd", "du", "stat", "file", "wc", "realpath", "readlink",
              "basename", "dirname", "ll", "la", "lsof-files"}
FIND_BASES = {"find", "fd", "fdfind", "locate", "mdfind"}
TEXT_FILTERS = {"jq", "yq", "awk", "gawk", "cut", "sort", "uniq", "tr", "column", "xargs", "paste",
                "join", "comm", "fold", "fmt", "tee"}
FS_OPS = {"mkdir", "mv", "cp", "rm", "rmdir", "touch", "chmod", "chown", "ln", "tar", "zip", "unzip",
          "gzip", "gunzip", "trash", "rsync", "scp", "install", "mktemp", "truncate", "dd", "xattr",
          "codesign", "chflags", "ditto"}
SHELL_BUILTINS = {"break", "continue", "((", "))", "[", "esac", "cd", "pushd", "popd", "export", "set", "unset", "source", ".", "alias", "true", "false",
                  ":", "exit", "wait", "return", "trap", "ulimit", "umask", "shift", "local", "declare",
                  "readonly", "hash", "eval", "then", "fi", "do", "done", "else", "if", "for", "while",
                  "in", "case", "esac", "function", "}", "{", "[[", "]]", "!"}
TRIVIAL = {"base64", "md5", "md5sum", "sha256sum", "code", "cursor", "echo", "printf", "sleep", "date", "pwd", "clear", "history", "env", "printenv", "whoami",
           "hostname", "id", "uname", "sw_vers", "uptime", "which", "type", "command", "test", "[",
           "ps", "pgrep", "pkill", "kill", "killall", "lsof", "top", "df", "free", "say", "afplay",
           "pbcopy", "pbpaste", "osascript", "launchctl", "defaults", "tput", "nohup", "disown", "jobs",
           "fg", "bg", "yes", "seq", "expr", "bc", "nc", "ping", "dig", "nslookup", "host", "caffeinate",
           "pmset", "networksetup", "scutil", "diskutil", "hdiutil", "security", "plutil", "mdutil", "ifconfig"}
OPEN_BASES = {"open", "xdg-open"}
INTERPRETERS = {"python", "python3", "python2", "node", "bun", "bunx", "deno", "ruby", "perl", "php", "lua",
                "tsx", "ts-node", "swift", "java", "kotlin", "dotnet", "julia", "Rscript", "elixir", "mix",
                "ghc", "runghc", "cargo-run", "uv", "uvx", "npx", "pnpx", "ipython", "pi", "claude", "codex",
                "devin", "herdr", "docker", "podman", "kubectl", "ssh", "systemctl", "brew", "cmux",
                "terminal-browser", "agent-browser", "acpx", "rmux", "tmux", "screen", "mise", "gtimeout",
                "sh", "bash", "zsh", "fish", "build-agent-config", "sync-agent-skills", "chezmoi", "vercel",
                "wrangler", "supabase", "convex", "xcrun", "simctl", "open-sim", "expo", "eas", "adb",
                "terraform", "ansible", "psql", "sqlite3", "redis-cli", "mongosh", "mysql", "dsh",
                "hyperfine", "cloc", "tokei", "liza", "omni-ee", "decision-map", "ffmpeg", "magick", "convert",
                "aws", "gcloud", "az", "bd", "infisical", "dokploy", "maestro", "android-remote", "orb", "omni",
                "cmux-agent-send", "pymobiledevice3", "ffprobe", "adb", "emulator", "suparun", "mongosh", "npm",
                "pnpm", "yarn", "rtk-gain", "gitnexus", "usable-git", "vps-run.sh", "agent-config", "codemap",
                "cmux-agent-triggerd", "opencode", "cursor-agent", "pwsh", "wezterm", "codex-acp", "depot", "niwa",
                "coderabbit", "sox", "ocu", "lightpanda", "arena.sh", "ollaya", "jav", "omni.js", "perl", "zero", "py",
                "montage", "sips", "screencapture", "pdftotext", "pdfinfo", "shasum", "openssl", "tailscale", "ollama",
                "doppler", "stripe", "agy", "trovex", "act", "repo-sync", "comet-cdp", "mdtoc", "orbctl", "nixpacks",
                "semble", "intent-map", "sample", "ddx", "gh-pages", "vercel", "railway", "fly", "flyctl", "heroku",
                "netlify", "firebase", "sentry-cli", "gt", "graphite", "pm2", "forever", "nodemon", "ts-node-dev",
                "afconvert", "exiftool", "identify", "plutil", "codesign", "spctl", "xattr", "mdfind", "pbcopy"}
TEST_BASES = {"pytest", "py.test", "jest", "vitest", "mocha", "rspec", "phpunit", "nextest", "tox", "ctest",
              "nosetests", "ava", "tap", "bats", "cargo-nextest", "cargo-mutants", "cypress", "playwright-test"}
BUILD_BASES = {"tsc", "tsgo", "eslint", "prettier", "biome", "ruff", "mypy", "pyright", "black", "flake8",
               "pylint", "rustc", "rustfmt", "clippy-driver", "swiftc", "gcc", "g++", "cc", "clang", "clang++",
               "cmake", "ninja", "xcodebuild", "shellcheck", "stylelint", "oxlint", "isort", "swiftlint",
               "swiftformat", "gofmt", "goimports", "esbuild", "webpack", "vite", "rollup", "parcel", "tsup",
               "turbo", "nx", "next", "nuxt", "astro", "hugo", "mdbook", "javac", "kotlinc", "scalac",
               "gradle", "gradlew", "mvn", "ant", "bazel", "buck", "meson", "msbuild", "cargo-clippy",
               "cargo-fmt", "taplo", "actionlint", "yamllint", "hadolint", "markdownlint", "typos", "sccache"}
RUNNER_BASES = {"bun", "npm", "pnpm", "yarn", "bunx", "npx", "pnpx", "deno", "uv", "poetry", "pipenv",
                "just", "make", "gmake", "task", "rake", "mise", "cargo", "go", "swift", "dotnet", "mix",
                "python", "python3", "node", "gradle", "gradlew", "mvn", "ant", "pip", "pip3", "pdm", "hatch", "rye"}
INSTALL_SUBS = {"install", "i", "add", "remove", "rm", "uninstall", "update", "upgrade", "up", "ci", "link",
                "unlink", "init", "create", "dedupe", "outdated", "audit", "pack", "publish", "login", "logout",
                "fetch", "clean", "tree", "metadata", "search", "yank", "new", "version", "login", "self"}
TEST_WORD = re.compile(r"(^|[:_./-])(tests?|specs?|e2e|unit|integration|jest|vitest|pytest|coverage|cov|mutants?|nextest)($|[:_./-])")
BUILD_WORD = re.compile(r"(^|[:_./-])(build|builds|compile|typecheck|type-check|types|tsc|tsgo|lint|linter|check|checks|"
                        r"fmt|format|clippy|bundle|dist|release|package|prepare|verify|gate|gates|validate|ci|"
                        r"doc|docs-build|analyze|codegen|gen|generate)($|[:_./-])")
SCRIPT_RUN_WORD = re.compile(r"(^|[:_./-])(dev|start|serve|server|watch|preview|run|demo|storybook)($|[:_./-])")
LOCAL_HOST = re.compile(r"(?:^|//)(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(?::\d+)?(?:/|$)")


def _args_without_flags(args, valued=()):
    out, skip = [], False
    for index, token in enumerate(args):
        if skip:
            skip = False
            continue
        if token == "--":
            out.extend(args[index + 1:])
            break
        if token.startswith("-") and token != "-":
            if token in valued and "=" not in token:
                skip = True
            continue
        out.append(token)
    return out


def _grep_pattern(args):
    """(pattern, fixed, regex_flag, paths) of a grep/rg command's arguments."""
    pattern, fixed, regex_flag = None, False, False
    rest, index = [], 0
    while index < len(args):
        token = args[index]
        if token in ("-e", "--regexp") and index + 1 < len(args):
            pattern = args[index + 1] if pattern is None else pattern + "|" + args[index + 1]
            index += 2
            continue
        if token.startswith("--regexp="):
            pattern = token.split("=", 1)[1]
        elif token in GREP_VALUE_FLAGS:
            index += 2
            continue
        elif token.startswith("--"):
            if token in ("--fixed-strings", "--fixed"):
                fixed = True
            elif token in ("--perl-regexp", "--extended-regexp", "--regexp"):
                regex_flag = True
        elif token.startswith("-") and token != "-" and not token.startswith("-e"):
            letters = token[1:]
            if "F" in letters:
                fixed = True
            if "P" in letters or "E" in letters:
                regex_flag = True
            if letters and letters[-1] in "ABCmfgtTjdD" and not letters.isdigit():
                index += 2
                continue
        elif token.startswith("-e") and len(token) > 2:
            pattern = token[2:] if pattern is None else pattern + "|" + token[2:]
        else:
            rest.append(token)
        index += 1
    if pattern is None and rest:
        pattern, rest = rest[0], rest[1:]
    return pattern, fixed, regex_flag, rest


def _grep_stage(base, args):
    if "--files" in args or "--files-with-matches-none" in args:
        return "list", "list-files"
    pattern, fixed, regex_flag, _ = _grep_pattern(args)
    if base == "fgrep":
        fixed = True
    if base == "egrep":
        regex_flag = True
    return "search", search_need(pattern, fixed, regex_flag)


def _file_args(args, valued=()):
    return [a for a in _args_without_flags(args, valued) if not re.match(r"^\d+(,\d+)?[pq]?$", a)]


def _read_stage(base, args):
    if base in ("head", "tail"):
        files = _file_args(args, {"-n", "-c", "--lines", "--bytes"})
        files = [f for f in files if not re.match(r"^[+-]?\d+$", f)]
        return ("read", read_need(files, region=True)) if files else ("other", "other")
    files = _file_args(args)
    if not files:
        return "other", "other"
    return "read", read_need(files)


def _sed_stage(args):
    positional = [t for t in args if not t.startswith("-")]
    files = positional[1:] if positional else []
    if not files:
        return "other", "other"
    return "read", read_need(files, region=True)


def _find_stage(base, args):
    if "-exec" in args or "-execdir" in args:
        for after in args[args.index("-exec" if "-exec" in args else "-execdir") + 1:][:2]:
            if os.path.basename(after) in GREP_BASES:
                tail = args[args.index(after) + 1:]
                pattern, fixed, regex_flag, _ = _grep_pattern([t for t in tail if t not in ("{}", ";", "+", "\\;")])
                return "search", search_need(pattern, fixed, regex_flag)
    has_name = any(a in ("-name", "-iname", "-path", "-ipath", "-regex", "-newer", "-mtime", "-size", "-perm",
                         "--extension", "-e", "--glob", "-g") for a in args)
    wants_dir = ("-type" in args and args[args.index("-type") + 1:][:1] == ["d"]) or "-t" in args and args[args.index("-t") + 1:][:1] == ["d"]
    if wants_dir or ("-maxdepth" in args and not has_name) or ("--max-depth" in args and not has_name):
        return "list", "list-tree"
    return "list", "list-files"


def _list_stage(base, args):
    if base in ("tree",) or "-R" in args or "--recursive" in args or any(
            t.startswith("-") and not t.startswith("--") and "R" in t for t in args):
        return "list", "list-tree"
    return "list", "list-files"


def _git_subcommand(args):
    """The git subcommand and its arguments, skipping global options."""
    index = 0
    while index < len(args):
        token = args[index]
        if token in ("-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path") and index + 1 < len(args):
            index += 2
        elif token.startswith("-"):
            index += 1
        else:
            return token, args[index + 1:]
    return "", []


GIT_HISTORY = {"log", "blame", "show", "shortlog", "reflog", "rev-list", "annotate", "whatchanged", "cherry",
               "range-diff", "show-branch", "name-rev", "describe", "bisect", "merge-base", "diff-tree", "show-ref"}
GIT_STATUS = {"status", "diff", "diff-index", "diff-files", "rev-parse", "remote", "config", "var",
              "worktree", "count-objects", "fsck", "check-ignore", "for-each-ref", "help", "version",
              "ls-remote", "verify-commit", "verify-tag", "symbolic-ref", "submodule"}
GIT_WRITE = {"add", "commit", "push", "pull", "checkout", "switch", "merge", "rebase", "reset", "restore",
             "stash", "cherry-pick", "revert", "clean", "rm", "mv", "apply", "am", "init", "clone", "fetch",
             "tag", "branch", "gc", "prune", "update-ref", "format-patch", "bundle", "notes", "filter-branch",
             "sparse-checkout", "lfs", "maintenance", "pack-refs", "repack", "commit-tree", "write-tree",
             "read-tree", "hash-object", "mktree", "update-index", "request-pull", "send-email"}


def _git_stage(args):
    sub, rest = _git_subcommand(args)
    if sub == "grep":
        pattern, fixed, regex_flag, _ = _grep_pattern([a for a in rest if a != "--"])
        return "search", search_need(pattern, fixed, regex_flag)
    if sub == "ls-files":
        return "list", "list-files"
    if sub == "ls-tree":
        return "list", "list-tree"
    if sub == "cat-file":
        return "read", "read-whole-file"
    if sub == "show" and any(re.match(r"^[\w./^~-]*:[^:\s]+", a) and not a.startswith("-") for a in rest):
        return "read", "read-whole-file"  # git show REV:path
    if sub in ("diff", "status") or sub in GIT_STATUS:
        if sub in ("remote", "config", "worktree", "submodule", "symbolic-ref", "var") and any(
                a in ("add", "remove", "rm", "set-url", "prune", "set", "--add", "--unset", "--set", "move",
                      "update", "init", "set-head", "rename") for a in rest):
            return "git-write", "other"
        return "git-read", "git-status"
    if sub == "branch":
        if any(a.startswith("-") and a not in ("-a", "-r", "-v", "-vv", "--list", "--show-current", "-l",
                                               "--merged", "--no-merged", "--contains", "--sort", "-av", "-avv") for a in rest) or _args_without_flags(rest):
            return "git-write", "other"
        return "git-read", "git-status"
    if sub == "tag" and not rest:
        return "git-read", "git-status"
    if sub == "tag" and rest[:1] in (["-l"], ["--list"]):
        return "git-read", "git-status"
    if sub == "stash" and rest[:1] in (["list"], ["show"]):
        return "git-read", "git-history"
    if sub in GIT_HISTORY:
        return "git-read", "git-history"
    if sub in GIT_WRITE or sub == "":
        return ("git-write", "other") if sub else ("git-read", "git-status")
    return "git-read", "git-status"


def _but_stage(args):
    sub = next((a for a in args if not a.startswith("-")), "")
    if sub in ("status", "diff", "show", "branch", "oplog", "info", "list"):
        return "git-read", "git-status"
    if sub in ("log", "blame"):
        return "git-read", "git-history"
    return "git-write", "other"


def _gh_stage(args):
    words = [a for a in args if not a.startswith("-")]
    if words[:1] == ["api"]:
        method = next((args[i + 1] for i, a in enumerate(args[:-1]) if a in ("-X", "--method")), "GET")
        return ("git-read", "other") if method.upper() == "GET" and "-f" not in args and "-F" not in args else ("git-write", "other")
    verb = words[1] if len(words) > 1 else ""
    if verb in ("view", "list", "checks", "status", "diff", "show", "watch", "search", "download", "get"):
        return "git-read", "other"
    if verb in ("create", "merge", "close", "comment", "edit", "review", "ready", "reopen", "delete", "push",
                "set", "run", "rerun", "cancel", "fork", "clone", "sync", "transfer", "lock", "unlock", "pin"):
        return "git-write", "other"
    return "git-read", "other"


PIXEL_NEEDS = {
    "find-symbol": "find-definition", "pack-context": "find-definition", "find-code": "find-definition",
    "resolve": "find-definition", "scope-task": "find-definition", "brief": "find-definition",
    "execution-brief": "find-definition", "plan": "find-definition", "search-meaning": "find-definition",
    "list-signatures": "read-whole-file", "repo-map": "list-tree", "list-areas": "list-tree",
    "list-flows": "list-tree", "coverage": "list-tree",
    "who-calls": "find-usages", "impact": "find-usages", "call-path": "find-usages", "cycles": "find-usages",
    "rename": "edit-code", "evaluate": "find-usages",
    "what-changed": "git-status", "repo-state": "git-status", "review-changes": "git-status",
    "review-gate": "git-status", "diff": "git-status", "list-branches": "git-status",
    "commit-history": "git-history", "search-history": "git-history", "file-history": "git-history",
    "dig-history": "git-history", "who-wrote": "git-history", "recall": "git-history",
    "web-search": "web-lookup",
    # subcommands of earlier pixel / gitpixel releases, still in the history
    "targets": "find-definition", "context": "find-definition", "ask": "find-definition", "symbol": "find-definition",
    "changes": "git-status", "review": "git-status", "inspect": "git-status",
    "history": "git-history", "history-search": "git-history", "rescue": "git-history",
    "excavate": "other", "sniper": "other", "task": "other", "publish": "other", "ship": "other", "branch": "other",
    "branches": "other", "reconcile": "other", "sync": "other", "rewrite": "other", "index": "other", "update": "other",
    "hook": "other", "mcp": "other", "ready": "other", "processes": "other",
}


def _pixel_stage(args):
    words = _args_without_flags(args)
    sub = words[0] if words else ""
    if sub in ("search-content", "search-like-rg", "search"):
        pattern, fixed, regex_flag, _ = _grep_pattern(args[args.index(sub) + 1:] if sub in args else args)
        return "pixel", search_need(pattern, fixed, regex_flag)
    if not words:
        return "pixel", "other"  # `pixel --help`, `pixel -V`
    if sub in PIXEL_NEEDS:
        return "pixel", PIXEL_NEEDS[sub]
    if sub in ("graph", "map"):  # the gitpixel front end
        return "pixel", "list-tree"
    if sub in ("build-index", "prepare-repo", "daemon", "install", "uninstall", "doctor", "self-update", "status",
               "config", "index-pack", "index-unpack", "rebuild-graph", "workspace", "reference", "note", "space",
               "index-stats", "action-log", "token-savings", "run-hook", "task-state", "commit", "push",
               "commit-and-push", "new-branch", "fast-forward", "fetch", "sync-branch", "squash-branch",
               "record-event", "classify", "classify-eval", "classify-history", "edit-env", "check-release",
               "plan-rollback", "list-errors", "audit", "run-recipe", "ultraflow", "flow", "help"):
        return "pixel", "other"
    return "pixel", "unknown"


def _script_kind(name):
    """test / build / run for a package script or make target name."""
    lowered = (name or "").lower()
    if TEST_WORD.search(lowered):
        return "test", "run-tests"
    if BUILD_WORD.search(lowered):
        return "build", "build-typecheck"
    return "run", "run-program"


def _runner_stage(base, args):
    """Classification of a package/task runner invocation, or None when it is not one."""
    words = _args_without_flags(args, {"--cwd", "-C", "--filter", "-w", "--workspace", "--prefix", "-p", "--package",
                                      "--manifest-path", "--target-dir", "-F", "--features", "--profile", "--bin",
                                      "-j", "--jobs", "--color", "--message-format"})
    sub = words[0] if words else ""
    rest = words[1:]
    if base in ("bunx", "npx", "pnpx", "uvx"):
        tool = sub if sub.startswith("@") else os.path.basename(sub)
        if tool in TEST_BASES or tool in ("jest", "vitest", "playwright") and "test" in rest:
            return "test", "run-tests"
        if tool in BUILD_BASES or tool in ("@typescript/native-preview", "typescript", "tsc", "tsgo", "@biomejs/biome"):
            return "build", "build-typecheck"
        return "run", "run-program"
    if base in ("bun", "npm", "pnpm", "yarn", "deno"):
        if sub in ("test", "t"):
            return "test", "run-tests"
        if sub in ("run", "run-script", "task", "x", "exec"):
            target = rest[0] if rest else ""
            if sub in ("x", "exec") and rest:
                return _runner_stage("bunx", rest)
            return _script_kind(target)
        if sub in ("build", "check", "lint", "fmt", "typecheck", "compile"):
            return "build", "build-typecheck"
        if sub in INSTALL_SUBS:
            return "run", "other"
        if sub in ("dev", "start", "serve", "watch", "preview"):
            return "run", "run-program"
        if base == "bun" and sub and (sub.endswith((".ts", ".js", ".tsx", ".mjs", ".cjs", ".mts")) or "/" in sub):
            return _script_file_kind(sub)
        if sub:
            return _script_kind(sub)
        return "run", "other"
    if base in ("cargo",):
        sub = next((a for a in args if not a.startswith("-") and not a.startswith("+")), "")
        if sub in ("test", "nextest", "t", "mutants", "llvm-cov", "tarpaulin", "insta", "fuzz", "miri"):
            return "test", "run-tests"
        if sub in ("build", "b", "check", "c", "clippy", "fmt", "doc", "rustc", "fix", "rustdoc", "bench", "deny",
                   "audit", "udeps", "machete", "expand", "hack", "semver-checks", "clean", "release", "xtask"):
            return "build", "build-typecheck"
        if sub in ("run", "r"):
            return "run", "run-program"
        if sub in INSTALL_SUBS:
            return "run", "other"
        return "run", "run-program"
    if base == "go":
        if sub == "test":
            return "test", "run-tests"
        if sub in ("build", "vet", "fmt", "generate", "mod", "install", "get", "list", "work", "fix"):
            return "build", "build-typecheck"
        return "run", "run-program"
    if base == "swift":
        if sub == "test":
            return "test", "run-tests"
        if sub in ("build", "package"):
            return "build", "build-typecheck"
        return "run", "run-program"
    if base == "dotnet":
        return ("test", "run-tests") if sub == "test" else ("build", "build-typecheck") if sub in ("build", "publish", "restore", "format") else ("run", "run-program")
    if base in ("gradle", "gradlew", "mvn", "ant"):
        return _script_kind(sub) if _script_kind(sub)[0] != "run" else ("build", "build-typecheck")
    if base in ("make", "gmake", "just", "task", "rake", "mise"):
        if base == "mise" and sub != "run":
            return "run", "other"
        target = rest[0] if (base == "mise" and rest) else sub
        if not target:
            return "build", "build-typecheck"
        kind = _script_kind(target)
        return ("build", "build-typecheck") if kind[0] == "run" and base in ("make", "gmake") else kind
    if base in ("uv", "poetry", "pipenv", "pdm", "hatch", "rye"):
        if sub == "run" and rest:
            return _python_stage(rest[0], rest[1:]) if os.path.basename(rest[0]).startswith("python") else _runner_by_tool(rest[0], rest[1:])
        if sub in INSTALL_SUBS or sub in ("sync", "lock", "pip", "venv", "shell"):
            return "run", "other"
        return "run", "run-program"
    if base in ("pip", "pip3"):
        return "run", "other"
    return None


def _runner_by_tool(tool, rest):
    tool = os.path.basename(tool)
    if tool in TEST_BASES:
        return "test", "run-tests"
    if tool in BUILD_BASES:
        return "build", "build-typecheck"
    return "run", "run-program"


def _script_file_kind(path):
    base = os.path.basename(path).lower()
    if re.match(r"^(test[_-]|.*[_.-]test\.|.*[_.-]spec\.|.*[_-]tests?\.)", base):
        return "test", "run-tests"
    return "run", "run-program"


def _python_stage(base, args):
    flags_done = False
    index = 0
    while index < len(args):
        token = args[index]
        if token == "-m" and index + 1 < len(args):
            module = args[index + 1]
            root = module.split(".")[0]
            if root in ("pytest", "unittest", "nose", "nose2", "doctest", "coverage", "tox") or TEST_WORD.search(module):
                return "test", "run-tests"
            if root in ("mypy", "ruff", "black", "flake8", "pyright", "compileall", "isort", "pylint", "build", "py_compile", "pip"):
                return ("run", "other") if root == "pip" else ("build", "build-typecheck")
            return "run", "run-program"
        if token in ("-c", "-"):
            return "run", "run-program"
        if token.startswith("-"):
            index += 1
            continue
        return _script_file_kind(token)
    return "run", "run-program"


def _node_stage(args):
    if "--test" in args:
        return "test", "run-tests"
    words = _args_without_flags(args, {"-r", "--require", "--import", "-e", "--eval", "-p", "--print"})
    if any(a in ("-e", "--eval", "-p", "--print") for a in args) or not words:
        return "run", "run-program"
    return _script_file_kind(words[0])


def _web_stage(base, args):
    urls = [a for a in args if "://" in a or re.match(r"^(localhost|127\.0\.0\.1)(:\d+)?(/|$)", a)]
    if urls and all(LOCAL_HOST.search(u) or u.startswith(("localhost", "127.0.0.1")) for u in urls):
        return "run", "run-program"
    return "web", "web-lookup"


def _write_kind(stage):
    """``edit`` / ``write`` / None when this one command line modifies a (non-temporary) file."""
    tokens = list(stage)
    base = os.path.basename(tokens[0]) if tokens else ""
    if base in ("sed", "perl", "ruby") and any(re.match(r"^-[A-Za-z]*i", t) or t.startswith("--in-place") for t in tokens[1:]):
        return "edit"
    if base == "patch" or (base == "git" and "apply" in tokens[1:3]):
        return "edit"
    if base == "tee":
        targets = [t for t in tokens[1:] if not t.startswith("-")]
        return "write" if any(not _temp_target(t) for t in targets) else None
    for index, token in enumerate(tokens):
        if token in (">", ">>") and index + 1 < len(tokens):
            target = tokens[index + 1]
            if target != "&" and not _temp_target(target):
                return "write"
    return None


def _temp_target(target):
    return (target.startswith(("/dev/", "/tmp/", "/private/tmp/", "/var/folders/", "$TMPDIR", "${TMPDIR", "&"))
            or "scratchpad" in target or target in ("/dev/null", "-"))


REDIRECT_OPS = {"<", "<<", "<<<", ">", ">>", ">&", "&>", ">|", "<&", "<>", "&>>"}


RTK_WRAPS = {"proxy", "err", "test", "summary", "smart", "pipe"}
RTK_NOOPS = {"env", "gain", "session", "discover", "telemetry", "learn", "recall", "init", "config",
             "cc-economics", "hook", "hook-audit"}
RTK_GLOBAL_FLAGS = {"-v", "-vv", "-vvv", "--verbose", "--ultra-compact", "--skip-env", "-u"}
RTK_READ_VALUED = {"-l", "--level", "-m", "--max-lines", "--head-lines", "--tail-lines"}


def _rtk_inner(rest):
    """Tokens of the command an ``rtk <sub> ...`` invocation stands for."""
    while rest and rest[0] in RTK_GLOBAL_FLAGS:  # `rtk -v git log`, `rtk --ultra-compact grep ...`
        rest = rest[1:]
    if not rest:
        return []
    sub, tail = rest[0], rest[1:]
    if sub in RTK_WRAPS:
        return tail
    if sub == "run":
        return ["__shell__", tail[0]] if tail else []
    if sub == "read":
        files, region, index = [], False, 0
        while index < len(tail):
            token = tail[index]
            if token in RTK_READ_VALUED:
                region = region or token != "-l" and token != "--level"
                index += 2
            elif token.startswith("-"):
                index += 1
            else:
                files.append(token)
                index += 1
        return ["head" if region else "cat"] + files
    if sub == "json":
        return ["cat"] + [t for t in tail if not t.startswith("-")]
    if sub == "log":
        return ["tail"] + [t for t in tail if not t.startswith("-")]
    if sub == "deps":
        return ["cat", "package.json"]
    if sub == "lint":
        return ["eslint"] + tail
    if sub == "format":
        return ["prettier"] + tail
    if sub == "diff":
        return ["diff"] + tail
    if sub in RTK_NOOPS:
        return ["true"]
    return rest


def strip_all(tokens):
    """Drop env assignments, ``timeout``-style wrappers and the ``rtk`` proxy (with its subcommands)."""
    tokens = [t.strip() for t in tokens]
    tokens = [t for i, t in enumerate(tokens) if t or i]
    while tokens and not tokens[0]:
        tokens.pop(0)
    while tokens:
        head = tokens[0]
        base = _EXC.WRAPPER_ALIASES.get(os.path.basename(head), os.path.basename(head))
        if _EXC.ASSIGNMENT.match(head) or re.match(r"^[A-Za-z_]\w*\+=", head):
            tokens.pop(0)
        elif base == "rtk":
            tokens = _rtk_inner(tokens[1:])
        elif base in _EXC.WRAPPER_SPECS:
            tokens = _EXC.skip_wrapper(base, tokens[1:])
        elif base in ("setsid", "caffeinate", "unbuffer", "chronic", "do", "then", "else", "elif", "!", "{", "(", "builtin",
                      "while", "until", "if", "time", "coproc"):
            tokens.pop(0)
        elif base == "script" and tokens[1:3] == ["-q", "/dev/null"]:
            tokens = tokens[3:]
        else:
            break
    return tokens


def strip_redirects(tokens):
    """The command tokens without redirections (``2>/dev/null``, ``<<EOF``, ``> out``)."""
    out, index = [], 0
    while index < len(tokens):
        token = tokens[index]
        if token in REDIRECT_OPS:
            if out and re.fullmatch(r"\d+", out[-1]):
                out.pop()
            index += 2
            continue
        out.append(token)
        index += 1
    return out


def _stage_class(tokens, later_stages):
    """(class, need) of one pipeline whose first command is ``tokens`` (wrappers already stripped)."""
    tokens = strip_redirects(tokens)
    if not tokens:
        return "other", "other"
    base = os.path.basename(tokens[0])
    args = tokens[1:]
    if base == "__shell__":
        return classify_bash(args[0]) if args else ("other", "other")
    if base in SHELLS:
        for position, token in enumerate(args):
            if not token.startswith("-"):
                break
            if SHELL_C.match(token):
                inner = args[position + 1:][:1]
                return classify_bash(inner[0]) if inner else ("other", "other")
        return ("run", "run-program") if args else ("other", "other")
    if base in SHELL_BUILTINS or base == "" or base.startswith(("$(", "-")):  # `-x`: an argument the shell lexer split off
        return "other", "other"
    if base in PIXEL_BINARIES:
        return _pixel_stage(args)
    if base == "git":
        verdict = _git_stage(args)
    elif base == "but":
        verdict = _but_stage(args)
    elif base == "gh":
        verdict = _gh_stage(args)
    elif base in GREP_BASES:
        verdict = _grep_stage(base, args)
    elif base in FIND_BASES:
        verdict = _find_stage(base, args)
    elif base == "sed":
        verdict = _sed_stage(args)
    elif base in READ_BASES:
        verdict = _read_stage(base, args)
    elif base in LIST_BASES:
        verdict = _list_stage(base, args)
    elif base in ("curl", "wget", "http", "https", "xh", "httpie", "aria2c"):
        verdict = _web_stage(base, args)
    elif base in OPEN_BASES:
        verdict = ("web", "web-lookup") if any("://" in a for a in args) else ("other", "other")
    elif base in TEST_BASES:
        verdict = ("test", "run-tests")
    elif base == "playwright":
        verdict = ("test", "run-tests") if "test" in args[:2] else ("run", "run-program")
    elif base in ("ast-grep", "sg"):
        verdict = ("search", "search-regex")
    elif base in ("scip-search", "codebase-memory-mcp", "semgrep"):
        verdict = ("search", search_need(next(iter(_args_without_flags(args)[1:2] or _args_without_flags(args)[:1]), "")))
    elif base == "apply_patch":
        verdict = ("edit", "edit-code")
    elif base == "jj":
        verdict = _but_stage(args)
    elif base == "stacklit":
        verdict = ("search", "find-definition") if args[:1] in (["find-module"], ["find"], ["search"]) else ("run", "run-program")
    elif base in ("bundle", "pod", "gem", "xcodegen", "fastlane"):
        verdict = ("build", "build-typecheck") if args[:1] in (["exec"], ["install"], ["build"]) or base in ("xcodegen", "fastlane") else ("run", "other")
    elif base == "pre-commit":
        verdict = ("build", "build-typecheck")
    elif base in ("xcodebuild", "docker", "podman") and "test" in args[:3]:
        verdict = ("test", "run-tests") if base == "xcodebuild" else ("run", "run-program")
    elif base in ("docker", "podman") and args[:1] == ["build"]:
        verdict = ("build", "build-typecheck")
    elif base in BUILD_BASES:
        verdict = ("build", "build-typecheck")
    elif base in RUNNER_BASES and base not in ("python", "python3", "node") and _runner_stage(base, args):
        verdict = _runner_stage(base, args)
    elif base.startswith("python") or base in ("python", "python3", "ipython"):
        verdict = _python_stage(base, args)
    elif base in ("node", "tsx", "ts-node"):
        verdict = _node_stage(args)
    elif base in ("jq", "yq", "awk", "gawk"):
        files = _args_without_flags(args)
        verdict = ("read", read_need(files[1:])) if len(files) >= 2 else ("other", "other")
    elif base in ("diff", "cmp", "colordiff", "delta", "icdiff"):
        verdict = ("read", "read-whole-file") if _args_without_flags(args) else ("other", "other")
    elif base in TEXT_FILTERS or base in FS_OPS or base in TRIVIAL:
        verdict = ("other", "other")
    elif base in INTERPRETERS:
        verdict = ("run", "run-program")
    elif tokens[0].startswith(("./", "/", "~", "../", "target/", "scripts/", "bin/", "node_modules/")):
        verdict = _script_file_kind(tokens[0])
    else:
        return "other", "unknown"
    # `find … | xargs grep`, `cat f | grep x`: a later grep makes the pipeline a search.
    if verdict[0] in ("list", "read", "git-read") and base not in GREP_BASES:
        for later in later_stages:
            later = strip_all(later)
            if later and os.path.basename(later[0]) in GREP_BASES:
                return _grep_stage(os.path.basename(later[0]), later[1:])
    return verdict


VAR_ASSIGN = re.compile(r"(?:^|[;&\n]\s*)([A-Za-z_]\w*)=(?:\"([^\"\n]{1,300})\"|'([^'\n]{1,300})'|([^\s;&|\"']{1,300}))")


def expand_command_variables(command):
    """Substitute ``NAME=value`` assignments of the line into a ``$NAME`` that starts a command (``GP="git -C x"; $GP status``)."""
    values = {}
    for match in VAR_ASSIGN.finditer(command):
        values[match.group(1)] = next(g for g in match.groups()[1:] if g is not None)
    for name, value in values.items():
        command = re.sub(r"(^|[;&|(\n]\s*)(?:\$" + name + r"\b|\$\{" + name + r"\})", lambda m: m.group(1) + value, command)
    return command


def newlines_to_semicolons(command):
    """Heredoc bodies dropped and every unquoted newline turned into ``;``.

    ``split_pipelines`` (excavation-count) leaves a newline between two commands as plain whitespace, which
    merges ``mv a b`` and the ``grep`` on the next line into one command with the first one's head.
    """
    text, out, quote, index = _EXC.strip_heredocs(command), [], None, 0
    while index < len(text):
        char = text[index]
        if char == "\\" and quote != "'" and index + 1 < len(text):
            out.append(" " if text[index + 1] == "\n" else char + text[index + 1])
            index += 2
            continue
        if quote:
            quote = None if char == quote else quote
        elif char in "\"'`":
            quote = char
        elif char == "\n":
            char = " ; "
        out.append(char)
        index += 1
    return "".join(out)


def classify_bash(command):
    """(class, need) of one shell command line: the highest-priority pipeline decides."""
    command = command if isinstance(command, str) else ""
    if "$" in command and "=" in command:
        command = expand_command_variables(command)
    pipelines = split_pipelines(newlines_to_semicolons(command)) if "\n" in command else split_pipelines(command)
    if pipelines is None:
        pipelines = [[command.split()]]
    best = None
    for pipeline in pipelines:
        stages = [strip_all(stage) for stage in pipeline]
        writing = [kind for kind in (_write_kind(stage) for stage in stages) if kind]
        if writing:
            verdict = (writing[0], "edit-code")
        else:
            verdict = _stage_class(stages[0] if stages else [], stages[1:])
        if best is None or PRIORITY.index(verdict[0]) < PRIORITY.index(best[0]) or (
                verdict[0] == best[0] == "other" and best[1] == "other" and verdict[1] == "unknown"):
            best = verdict
    return best or ("other", "other")


# --------------------------------------------------------------------------
# Tool-level classification
# --------------------------------------------------------------------------

READ_TOOLS = {"read", "notebookread", "view", "readfile", "read_file", "open_file", "cat", "read_many_files"}
SEARCH_TOOLS = {"grep", "search", "rg", "search-content", "search_content", "search_files", "ripgrep",
                "grep_search", "codebase_search", "search_code"}
GLOB_TOOLS = {"glob", "find", "find_files", "file_search", "glob_search"}
LS_TOOLS = {"ls", "list", "list_dir", "list_directory", "listdirectory", "tree"}
EDIT_TOOLS = {"edit", "multiedit", "notebookedit", "str_replace_editor", "str_replace", "replace", "apply_patch",
              "patch", "str_replace_based_edit_tool", "edit_file"}
WRITE_TOOLS = {"write", "create", "create_file", "write_file", "overwrite"}
BASH_TOOLS = {"bash", "shell", "exec_command", "local_shell", "run_command", "execute_bash", "terminal", "sh"}
WEB_TOOLS = {"webfetch", "websearch", "web_search", "google_search", "web__run", "fetch", "web_fetch", "web_search_call",
             "browse", "web"}
OTHER_TOOLS = {"agent", "task", "subagent", "scout", "spawn_agent", "taskcreate", "taskupdate", "taskget", "tasklist",
               "taskoutput", "taskstop", "todowrite", "todoread", "todo", "plan", "decide", "skill", "toolsearch",
               "askuserquestion", "exitplanmode", "enterplanmode", "monitor", "schedulewakeup", "sendmessage",
               "listagents", "pushnotification", "senduserfile", "artifact", "update_plan", "wait", "wait_agent",
               "sleep", "send_message", "followup_task", "interrupt_agent", "close_agent", "list_agents",
               "request_user_input", "request_user_input_async", "enterworktree", "exitworktree", "gd",
               "generate_image", "view_image", "list_mcp_resources", "read_mcp_resource", "list_mcp_resource_templates",
               "tool_search_call", "tool_search_output", "ask_pixel_files", "ask_pixel_file_bool", "ask_jev_files",
               "macp_poll", "macp_send_channel", "bashoutput", "killshell", "killbash", "write_stdin", "shell_job",
               "ctx_search", "ctx_batch_execute", "ctx_index", "update_goal", "get_goal", "create_goal",
               "wait_threads", "multi_agent_v1", "croncreate", "crondelete", "cronlist", "readnotifications",
               "monitor", "taskoutput", "enterworktree", "send_input", "register", "read_thread_terminal", "curr_time",
               "imagegen", "list_threads", "get_thread", "request_permissions"}


def _first(inp, *keys):
    for key in keys:
        value = inp.get(key)
        if isinstance(value, str) and value:
            return value
    return ""


def _path_of(inp):
    path = _first(inp, "file_path", "path", "filePath", "filename", "file", "notebook_path", "target_file", "uri")
    if path:
        return path
    paths = inp.get("paths") or inp.get("files")
    return paths[0] if isinstance(paths, list) and paths and isinstance(paths[0], str) else ""


def _has_range(inp):
    return any(inp.get(k) not in (None, "", 0) for k in ("offset", "limit", "start_line", "end_line", "line", "range",
                                                          "view_range", "lines", "startLine", "endLine", "line_start",
                                                          "line_end", "from", "to", "head", "tail", "max_lines"))


def _mcp_stage(name):
    """Rules for ``mcp__server__tool`` names (and Codex namespaced tools)."""
    lowered = name.lower()
    parts = lowered.split("__")
    server = parts[1] if len(parts) > 2 else ""
    tool = parts[-1]
    if any(k in server for k in ("browser", "computer-use", "chrome", "playwright", "puppeteer", "simulator", "terminal", "ccd_", "scheduled", "visualize", "annotator")) \
            or any(k in tool for k in ("screenshot", "navigate", "click", "computer_batch", "browser_batch", "tabs_", "read_page", "get_page_text", "javascript_tool", "mark_chapter", "spawn_task", "show_widget", "read_me")):
        return "other", "other"
    if any(k in server for k in ("macp", "codex_app", "agent-relay", "relay")):
        return "other", "other"
    if "repl" in server or tool in ("js", "python", "node", "eval", "execute_code", "run_code"):
        return "run", "run-program"
    if re.search(r"jira|confluence|atlassian", tool) or tool.startswith("stripe_"):
        return ("web", "web-lookup") if re.match(r"^_?(get|search|list|read|fetch|query|stripe_api_(read|search|details)|stripe_analytics|search_stripe)", tool) or "search" in tool else ("other", "other")
    if tool.endswith(("api_impact", "_impact")):
        return "search", "find-usages"
    if tool.startswith("notion-"):
        return ("web", "web-lookup") if re.match(r"^notion-(fetch|search|get|query|list)", tool) else ("other", "other")
    if tool in ("trace_path", "trace_call_path", "get_callers", "get_callees"):
        return "search", "find-usages"
    if "usable_git" in server or "usable-git" in server:
        return "git-read", "git-status"
    if any(k in server for k in ("context7", "docs")) or tool in ("query-docs", "search_docs", "resolve-library-id", "resolve_library_id", "get-library-docs"):
        return "web", "read-docs"
    if any(k in server for k in ("github", "stdio-server")):
        if re.match(r"^(create|update|merge|push|fork|add_)", tool):
            return "git-write", "other"
        if tool == "search_code":
            return "search", "search-string"
        if tool in ("get_file_contents",):
            return "read", "read-whole-file"
        if tool in ("list_commits",):
            return "git-read", "git-history"
        return "git-read", "other"
    if any(k in server for k in ("atlassian", "jira", "confluence", "notion", "stripe", "supabase", "linear", "slack", "mcp-registry")):
        return ("web", "web-lookup") if re.match(r"^(get|search|list|read|fetch|query|describe|atlassianuserinfo|discover|execute_sql_read)", tool.lstrip("_")) else ("other", "other")
    if True:  # code-intelligence servers (gitnexus, codebase-memory, serena, context-engine ...)
        if tool in ("impact", "callers", "references", "find_references", "usages", "who_calls", "trace_call_path"):
            return "search", "find-usages"
        if tool in ("context", "get_symbol", "symbol", "find_symbol", "definition", "go_to_definition", "get_code_snippet", "get_architecture", "explain"):
            return "search", "find-definition"
        if tool in ("query", "search_graph", "search", "search_code", "grep", "find", "semantic_search", "codebase_search"):
            return "search", "search-string"
        if tool in ("detect_changes", "changes", "diff"):
            return "git-read", "git-status"
        if tool in ("list_projects", "list_repos", "get_graph_schema", "index_status", "list_files", "tree"):
            return "list", "list-tree"
        if tool in ("read_file", "get_file", "read"):
            return "read", "read-whole-file"
    return "other", "unknown"


def classify(tool, tool_input):
    """(class, need) of one tool call; ``tool_input`` is the parsed argument dict (or raw string)."""
    name = (tool or "").strip()
    lowered = name.lower()
    inp = tool_input if isinstance(tool_input, dict) else {}
    if isinstance(tool_input, str):
        inp = {"command": tool_input} if lowered in BASH_TOOLS else {"input": tool_input}

    if lowered == "exec":  # a Codex script without a tools.* call: plain computation in the sandbox
        return "run", "run-program"
    if lowered == "lsp":
        operation = _first(inp, "operation", "action", "command").lower()
        if "reference" in operation or "caller" in operation or "implementation" in operation:
            return "search", "find-usages"
        if "symbol" in operation:
            return "list", "list-tree"
        return "search", "find-definition"
    if lowered in BASH_TOOLS:
        command = _first(inp, "command", "cmd", "script", "input")
        if not command and isinstance(inp.get("command"), list):
            command = " ".join(str(x) for x in inp["command"])
        return classify_bash(command)
    if lowered == "shell_job":
        command = _first(inp, "command", "cmd")
        return classify_bash(command) if command else ("run", "other")
    if lowered == "write_stdin":
        return "run", "other"
    if lowered in READ_TOOLS:
        return "read", read_need([_path_of(inp)], region=_has_range(inp))
    if lowered in ("str_replace_editor", "str_replace_based_edit_tool"):
        command = inp.get("command")
        if command == "view":
            return "read", read_need([_path_of(inp)], region=_has_range(inp))
        return ("write", "edit-code") if command == "create" else ("edit", "edit-code")
    if lowered in EDIT_TOOLS:
        if lowered == "apply_patch":
            patch = _first(inp, "patch", "input")
            if patch and "*** Update File" not in patch and "*** Delete File" not in patch and "*** Add File" in patch:
                return "write", "edit-code"
        return "edit", "edit-code"
    if lowered in WRITE_TOOLS:
        return "write", "edit-code"
    if lowered in SEARCH_TOOLS:
        pattern = _first(inp, "pattern", "query", "regex", "text", "search", "q", "search_query")
        if lowered == "search_code":
            pattern = pattern or _first(inp, "pattern")
        fixed = inp.get("regex") is False or inp.get("literal") is True or inp.get("fixed") is True
        return "search", search_need(pattern, fixed=bool(fixed))
    if lowered in GLOB_TOOLS:
        return "list", "list-files"
    if lowered in LS_TOOLS:
        return "list", "list-tree" if lowered == "tree" or inp.get("recursive") else "list-files"
    if lowered in WEB_TOOLS or lowered == "web_search_call":
        url = _first(inp, "url")
        if lowered in ("webfetch", "web_fetch", "fetch") and re.search(r"(docs?\.|/docs?/|developer\.|readme|/reference|/guide|/api/|\.dev/|github\.com/[^/]+/[^/]+/(blob|tree))", url):
            return "web", "read-docs"
        return "web", "web-lookup"
    if lowered == "pixel":
        action = _first(inp, "action", "command", "subcommand")
        query = _first(inp, "query", "pattern")
        if action in ("search", "search-content", "search_content"):
            return "pixel", search_need(query)
        if action in ("list", "tree", "repo-map"):
            return "pixel", "list-tree"
        if action in ("read", "list-signatures", "signatures"):
            return "pixel", "read-whole-file"
        return _pixel_stage([action.replace("_", "-")] + ([query] if query else []))
    if lowered == "pixel_project":
        action = _first(inp, "action")
        return "pixel", {"impact": "find-usages", "who_calls": "find-usages", "callers": "find-usages", "find_symbol": "find-definition",
                         "pack_context": "find-definition", "overview": "list-tree"}.get(action, "other" if action else "unknown")
    if lowered in ("git_diff", "git_status"):
        return "git-read", "git-status"
    if lowered in ("git_log", "git_blame", "git_show"):
        return "git-read", "git-history"
    if lowered in ("git_checkout", "git_commit", "git_push", "git_add"):
        return "git-write", "other"
    if lowered in OTHER_TOOLS or lowered.startswith(("multi_agent_v1", "collaboration")):
        return "other", "other"
    if lowered.split("__")[-1] in OTHER_TOOLS and "__" in lowered:
        return "other", "other"
    if lowered.startswith("mcp__") or "__" in lowered:
        return _mcp_stage(lowered)
    if lowered.startswith("mcp") or name[:1] == "_":
        return _mcp_stage("mcp__" + lowered)
    return "other", "unknown"


def classify_call(tool, tool_input):
    """Same as :func:`classify`; JSON text input is parsed first."""
    if isinstance(tool_input, str):
        try:
            parsed = json.loads(tool_input)
            tool_input = parsed if isinstance(parsed, (dict, list)) else tool_input
        except ValueError:
            pass
    return classify(tool, tool_input)
