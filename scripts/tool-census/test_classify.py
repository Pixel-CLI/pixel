#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Rule tests of classify.py on literal examples.

    python3 -I scripts/tool-census/test_classify.py
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import classify  # noqa: E402


def bash(command):
    return classify.classify("Bash", {"command": command})


class NativeTools(unittest.TestCase):
    def test_read_whole_and_region(self):
        self.assertEqual(classify.classify("Read", {"file_path": "/r/src/lib.rs"}), ("read", "read-whole-file"))
        self.assertEqual(classify.classify("Read", {"file_path": "/r/src/lib.rs", "offset": 120, "limit": 80}),
                         ("read", "read-region"))
        self.assertEqual(classify.classify("read", {"path": "crates/pixel-install/src/config.rs", "offset": 1, "limit": 2000}),
                         ("read", "read-region"))

    def test_read_config_and_docs(self):
        self.assertEqual(classify.classify("Read", {"file_path": "/r/Cargo.toml"}), ("read", "read-config"))
        self.assertEqual(classify.classify("Read", {"file_path": "/r/package.json"}), ("read", "read-config"))
        self.assertEqual(classify.classify("Read", {"file_path": "/r/.github/workflows/ci.yml"}), ("read", "read-config"))
        self.assertEqual(classify.classify("Read", {"file_path": "/r/README.md"}), ("read", "read-docs"))
        self.assertEqual(classify.classify("Read", {"file_path": "/r/docs/threat-model.md", "limit": 50}), ("read", "read-docs"))

    def test_grep_tool(self):
        self.assertEqual(classify.classify("Grep", {"pattern": "fn resolve_calls"}), ("search", "find-definition"))
        self.assertEqual(classify.classify("Grep", {"pattern": "resolve_calls"}), ("search", "find-usages"))
        self.assertEqual(classify.classify("Grep", {"pattern": "TODO"}), ("search", "search-string"))
        self.assertEqual(classify.classify("Grep", {"pattern": "error.*timeout\\d+"}), ("search", "search-regex"))
        self.assertEqual(classify.classify("Grep", {"pattern": "class\\s+UserService"}), ("search", "find-definition"))

    def test_glob_and_ls(self):
        self.assertEqual(classify.classify("Glob", {"pattern": "**/*.rs"}), ("list", "list-files"))
        self.assertEqual(classify.classify("ls", {"path": "src"}), ("list", "list-files"))

    def test_edit_and_write(self):
        self.assertEqual(classify.classify("Edit", {"file_path": "a.rs", "old_string": "a", "new_string": "b"}), ("edit", "edit-code"))
        self.assertEqual(classify.classify("MultiEdit", {"file_path": "a.rs"}), ("edit", "edit-code"))
        self.assertEqual(classify.classify("Write", {"file_path": "a.rs", "content": "x"}), ("write", "edit-code"))
        self.assertEqual(classify.classify("apply_patch", {"patch": "*** Begin Patch\n*** Update File: a.rs\n"}), ("edit", "edit-code"))
        self.assertEqual(classify.classify("apply_patch", {"patch": "*** Begin Patch\n*** Add File: a.rs\n"}), ("write", "edit-code"))

    def test_web_and_meta_tools(self):
        self.assertEqual(classify.classify("WebSearch", {"query": "rust"}), ("web", "web-lookup"))
        self.assertEqual(classify.classify("WebFetch", {"url": "https://docs.rs/serde/latest/serde/"}), ("web", "read-docs"))
        self.assertEqual(classify.classify("WebFetch", {"url": "https://news.example.com/a"}), ("web", "web-lookup"))
        self.assertEqual(classify.classify("Agent", {"prompt": "x"}), ("other", "other"))
        self.assertEqual(classify.classify("TaskUpdate", {}), ("other", "other"))
        self.assertEqual(classify.classify("ToolSearch", {"query": "x"}), ("other", "other"))
        self.assertEqual(classify.classify("mcp__Claude_Browser__computer", {}), ("other", "other"))
        self.assertEqual(classify.classify("totally_new_tool", {}), ("other", "unknown"))

    def test_mcp_code_intel(self):
        self.assertEqual(classify.classify("impact", {"target": "main"}), ("other", "unknown"))  # bare names are not namespaced
        self.assertEqual(classify.classify("mcp__gitnexus__impact", {"target": "main"}), ("search", "find-usages"))
        self.assertEqual(classify.classify("mcp__gitnexus__context", {"name": "X"}), ("search", "find-definition"))
        self.assertEqual(classify.classify("mcp__gitnexus__detect_changes", {}), ("git-read", "git-status"))
        self.assertEqual(classify.classify("mcp__context7__query-docs", {}), ("web", "read-docs"))
        self.assertEqual(classify.classify("mcp__github__create_issue", {}), ("git-write", "other"))

    def test_pi_tools(self):
        self.assertEqual(classify.classify("bash", {"command": "ls src"}), ("list", "list-files"))
        self.assertEqual(classify.classify("search-content", {"path": "", "query": "onAllSettled("}), ("search", "find-usages"))
        self.assertEqual(classify.classify("rg", {"path": "a.md", "query": "## New prompts"}), ("search", "search-string"))
        self.assertEqual(classify.classify("pixel", {"action": "search-content", "query": "PIXEL_EXECUTABLE"}), ("pixel", "find-usages"))
        self.assertEqual(classify.classify("pixel_project", {"action": "impact", "symbol": "x"}), ("pixel", "find-usages"))
        self.assertEqual(classify.classify("shell_job", {"action": "wait", "jobId": "j"}), ("run", "other"))
        self.assertEqual(classify.classify("git_diff", {}), ("git-read", "git-status"))


class BashSearchReadList(unittest.TestCase):
    def test_grep_family(self):
        self.assertEqual(bash("grep -rn 'fn parse_config' crates/"), ("search", "find-definition"))
        self.assertEqual(bash("rg -n 'parse_config' crates/"), ("search", "find-usages"))
        self.assertEqual(bash("rg -n 'parse_config|load_config' src"), ("search", "find-usages"))
        self.assertEqual(bash("rg -F 'a.b(c)' src"), ("search", "search-string"))
        self.assertEqual(bash("grep -rn \"error\" ."), ("search", "search-string"))
        self.assertEqual(bash("grep -E 'foo[0-9]+bar' f"), ("search", "search-regex"))
        self.assertEqual(bash("rg -e 'def handle' -e 'class Handler' ."), ("search", "find-definition"))
        self.assertEqual(bash("rg -A3 -B1 -g '*.rs' 'ReadError' ."), ("search", "find-usages"))
        self.assertEqual(bash("git grep -n 'TODO' -- '*.rs'"), ("search", "search-string"))

    def test_rtk_proxy_is_transparent(self):
        self.assertEqual(bash("rtk grep -rn 'resolve_calls' crates"), ("search", "find-usages"))
        self.assertEqual(bash("rtk read Cargo.toml"), ("read", "read-config"))
        self.assertEqual(bash("rtk git status --short"), ("git-read", "git-status"))
        self.assertEqual(bash("rtk ls crates"), ("list", "list-files"))

    def test_rtk_subcommands(self):
        self.assertEqual(bash("rtk err bun run test --runInBand src/a.test.tsx"), ("test", "run-tests"))
        self.assertEqual(bash("rtk test cargo test -p pixel"), ("test", "run-tests"))
        self.assertEqual(bash("rtk run 'cd /r && bunx @typescript/native-preview --noEmit'"), ("build", "build-typecheck"))
        self.assertEqual(bash("rtk read -l aggressive crates/pixel/src/main.rs"), ("read", "read-whole-file"))
        self.assertEqual(bash("rtk read -l aggressive package.json"), ("read", "read-config"))
        self.assertEqual(bash("rtk read --tail-lines 40 build.log"), ("read", "read-region"))
        self.assertEqual(bash("rtk json package.json"), ("read", "read-config"))
        self.assertEqual(bash("rtk log build.log"), ("read", "read-region"))
        self.assertEqual(bash("rtk proxy /usr/bin/time -p pixel --help"), ("pixel", "other"))
        self.assertEqual(bash("rtk gitpixel symbol Offer . --json"), ("pixel", "find-definition"))

    def test_rtk_global_options_and_wrappers(self):
        self.assertEqual(bash("rtk -v git log --oneline"), ("git-read", "git-history"))
        self.assertEqual(bash("rtk --ultra-compact grep -rn 'resolve_calls' crates"), ("search", "find-usages"))
        self.assertEqual(bash("rtk wc -l src/main.rs"), ("list", "list-files"))
        self.assertEqual(bash("rtk timeout 30 env FOO=1 cargo test"), ("test", "run-tests"))
        self.assertEqual(bash("rtk proxy env -u X bun run build"), ("build", "build-typecheck"))
        self.assertEqual(bash("rtk uv run pytest -q"), ("test", "run-tests"))
        self.assertEqual(bash("rtk npx tsc --noEmit"), ("build", "build-typecheck"))
        self.assertEqual(bash("rtk bunx vitest run"), ("test", "run-tests"))
        self.assertEqual(bash("rtk setsid nohup python3 serve.py"), ("run", "run-program"))

    def test_loops_conditions_and_variables(self):
        self.assertEqual(bash("until curl -s http://localhost:3000/health; do sleep 2; done"), ("run", "run-program"))
        self.assertEqual(bash("while pgrep -f server; do sleep 1; done"), ("other", "other"))
        self.assertEqual(bash("for f in a b; do rtk read $f; done"), ("read", "read-whole-file"))
        self.assertEqual(bash("if git diff --quiet; then echo clean; fi"), ("git-read", "git-status"))
        self.assertEqual(bash('GP="git -C /repo"; $GP status --short'), ("git-read", "git-status"))
        self.assertEqual(bash('W=/repo/web; cd $W && rtk bun run build'), ("build", "build-typecheck"))
        self.assertEqual(bash("ARGS+=x; cargo test"), ("test", "run-tests"))
        self.assertEqual(bash("\nmv a b\ngrep -n 'x' f"), ("search", "search-string"))

    def test_multiline_commands_are_several_commands(self):
        self.assertEqual(bash("mkdir -p out\nrtk grep -rn 'resolve_calls' crates"), ("search", "find-usages"))
        self.assertEqual(bash("echo start\ncat > /tmp/x <<'EOF'\nbody\nEOF\nrtk git log --oneline"), ("git-read", "git-history"))
        self.assertEqual(bash("echo 'a\nb' && ls"), ("list", "list-files"))  # a newline inside quotes stays
        self.assertEqual(bash("cargo build \\\n  --release"), ("build", "build-typecheck"))

    def test_more_tools_hiding_a_need(self):
        self.assertEqual(bash("scip-search 'resolve_calls'"), ("search", "find-usages"))
        self.assertEqual(bash("pre-commit run --all-files"), ("build", "build-typecheck"))
        self.assertEqual(bash("rtk ast-grep -p 'fn $A()' src"), ("search", "search-regex"))
        self.assertEqual(bash("cursor-agent -p 'x'"), ("run", "run-program"))
        self.assertEqual(classify.classify("LSP", {"operation": "findReferences"}), ("search", "find-usages"))
        self.assertEqual(classify.classify("LSP", {"operation": "goToDefinition"}), ("search", "find-definition"))
        self.assertEqual(classify.classify("exec", {"script": "text(load('x'))"}), ("run", "run-program"))
        self.assertEqual(classify.classify("pixel", {"action": "find_code", "query": "x"}), ("pixel", "find-definition"))
        self.assertEqual(classify.classify("pixel", {"action": "list_areas"}), ("pixel", "list-tree"))
        self.assertEqual(classify.classify("mcp__3346-uuid__searchJiraIssuesUsingJql", {}), ("web", "web-lookup"))
        self.assertEqual(classify.classify("mcp__gitnexus__api_impact", {}), ("search", "find-usages"))
        self.assertEqual(classify.classify("CronCreate", {}), ("other", "other"))

    def test_reads(self):
        self.assertEqual(bash("cat src/main.rs"), ("read", "read-whole-file"))
        self.assertEqual(bash("cat package.json"), ("read", "read-config"))
        self.assertEqual(bash("cat README.md CONTRIBUTING.md"), ("read", "read-docs"))
        self.assertEqual(bash("sed -n '10,40p' src/main.rs"), ("read", "read-region"))
        self.assertEqual(bash("head -n 50 src/main.rs"), ("read", "read-region"))
        self.assertEqual(bash("tail -20 build.log"), ("read", "read-region"))
        self.assertEqual(bash("cat <<'EOF'\nhello\nEOF"), ("other", "other"))

    def test_lists(self):
        self.assertEqual(bash("ls -la crates"), ("list", "list-files"))
        self.assertEqual(bash("ls -R crates | head -50"), ("list", "list-tree"))
        self.assertEqual(bash("tree -L 2 crates"), ("list", "list-tree"))
        self.assertEqual(bash("find . -name '*.rs' -not -path './target/*'"), ("list", "list-files"))
        self.assertEqual(bash("find crates -maxdepth 2 -type d"), ("list", "list-tree"))
        self.assertEqual(bash("rg --files crates"), ("list", "list-files"))
        self.assertEqual(bash("git ls-files crates"), ("list", "list-files"))

    def test_later_grep_makes_a_search(self):
        self.assertEqual(bash("find . -name '*.rs' | xargs grep -n 'resolve_calls'"), ("search", "find-usages"))
        self.assertEqual(bash("cat src/main.rs | grep -n 'fn main'"), ("search", "find-definition"))
        self.assertEqual(bash("find . -name '*.py' -exec grep -l 'import requests' {} ;"), ("search", "search-string"))

    def test_compound_command_takes_the_highest_priority(self):
        self.assertEqual(bash("cd /repo && cargo test -p pixel-graph"), ("test", "run-tests"))
        self.assertEqual(bash("cd /repo && cat Cargo.toml && ls"), ("read", "read-config"))
        self.assertEqual(bash("timeout 30 rg -n 'x' src"), ("search", "search-string"))
        self.assertEqual(bash("bash -lc \"rg -n 'resolve_calls' crates\""), ("search", "find-usages"))


class BashGitBuildRun(unittest.TestCase):
    def test_git(self):
        self.assertEqual(bash("git log --oneline -20"), ("git-read", "git-history"))
        self.assertEqual(bash("git blame -L 1,20 src/main.rs"), ("git-read", "git-history"))
        self.assertEqual(bash("git show abc123"), ("git-read", "git-history"))
        self.assertEqual(bash("git show HEAD:src/main.rs"), ("read", "read-whole-file"))
        self.assertEqual(bash("git status --short"), ("git-read", "git-status"))
        self.assertEqual(bash("git -C /repo diff --stat"), ("git-read", "git-status"))
        self.assertEqual(bash("git branch -a"), ("git-read", "git-status"))
        self.assertEqual(bash("git commit -m 'x'"), ("git-write", "other"))
        self.assertEqual(bash("git checkout -b feat/x"), ("git-write", "other"))
        self.assertEqual(bash("git branch -D old"), ("git-write", "other"))
        self.assertEqual(bash("git push origin main"), ("git-write", "other"))
        self.assertEqual(bash("gh pr view 12 --json state"), ("git-read", "other"))
        self.assertEqual(bash("gh pr create --title x"), ("git-write", "other"))
        self.assertEqual(bash("but status"), ("git-read", "git-status"))
        self.assertEqual(bash("but commit -m x"), ("git-write", "other"))

    def test_tests(self):
        for command in ("cargo test -p pixel", "cargo nextest run", "pytest -q tests/", "python3 -m pytest x",
                        "python3 -I -m unittest discover", "bun test", "bun run test", "npm test", "npm run test:unit",
                        "go test ./...", "make test", "just test-all", "vitest run", "bunx jest", "python3 -I scripts/tool-census/test_classify.py",
                        "cargo mutants --in-diff d.diff", "xcodebuild test -scheme X"):
            self.assertEqual(bash(command), ("test", "run-tests"), command)

    def test_builds(self):
        for command in ("cargo build --release", "cargo check -p pixel", "cargo clippy --all-targets", "cargo fmt --check",
                        "bun run build", "npm run typecheck", "tsc --noEmit", "bunx @typescript/native-preview --noEmit",
                        "go build ./...", "make", "make build", "eslint src", "ruff check .", "bun run lint", "cargo doc --no-deps",
                        "docker build -t x ."):
            self.assertEqual(bash(command), ("build", "build-typecheck"), command)

    def test_runs(self):
        for command in ("python3 scripts/run.py", "node server.js", "bun run dev", "cargo run -- --help", "./target/debug/mytool status",
                        "python3 -c 'print(1)'", "bun x.ts", "npm run start", "docker ps", "ssh host uptime"):
            self.assertEqual(bash(command)[0], "run", command)
        self.assertEqual(bash("python3 scripts/run.py"), ("run", "run-program"))
        self.assertEqual(bash("curl http://localhost:3000/health"), ("run", "run-program"))
        self.assertEqual(bash("bun install"), ("run", "other"))
        self.assertEqual(bash("cargo install ripgrep"), ("run", "other"))

    def test_pixel_cli(self):
        self.assertEqual(bash("pixel find-symbol resolve_calls"), ("pixel", "find-definition"))
        self.assertEqual(bash("pixel who-calls resolve_calls"), ("pixel", "find-usages"))
        self.assertEqual(bash("pixel search-content -F 'resolve_calls' crates"), ("pixel", "find-usages"))
        self.assertEqual(bash("pixel search-content -F 'could not open' crates"), ("pixel", "search-string"))
        self.assertEqual(bash("rtk pixel search-content 'resolve_calls' crates"), ("pixel", "find-usages"))
        self.assertEqual(bash("pixel list-signatures app.ts"), ("pixel", "read-whole-file"))
        self.assertEqual(bash("pixel commit-history . -n 5"), ("pixel", "git-history"))
        self.assertEqual(bash("pixel doctor ."), ("pixel", "other"))
        self.assertEqual(bash("pixel-dev who-calls x"), ("pixel", "find-usages"))

    def test_web(self):
        self.assertEqual(bash("curl -s https://api.github.com/repos/x/y"), ("web", "web-lookup"))
        self.assertEqual(bash("wget https://example.com/a.tgz"), ("web", "web-lookup"))

    def test_writes(self):
        self.assertEqual(bash("echo hi > notes.txt"), ("write", "edit-code"))
        self.assertEqual(bash("cat > src/a.rs <<'EOF'\nfn a() {}\nEOF"), ("write", "edit-code"))
        self.assertEqual(bash("sed -i '' 's/a/b/' src/a.rs"), ("edit", "edit-code"))
        self.assertEqual(bash("echo hi | tee out.txt"), ("write", "edit-code"))
        # temp-file redirects and /dev/null are not file edits
        self.assertEqual(bash("cargo test > /tmp/out.log 2>&1"), ("test", "run-tests"))
        self.assertEqual(bash("rg foo src 2>/dev/null"), ("search", "search-string"))

    def test_filesystem_and_trivia(self):
        self.assertEqual(bash("mkdir -p out && cp a b"), ("other", "other"))
        self.assertEqual(bash("sleep 5"), ("other", "other"))
        self.assertEqual(bash("echo done"), ("other", "other"))
        self.assertEqual(bash("cd /repo"), ("other", "other"))

    def test_unknown_bucket(self):
        self.assertEqual(bash("frobnicate --all"), ("other", "unknown"))
        self.assertEqual(bash(""), ("other", "other"))


class CodexAndSearchNeeds(unittest.TestCase):
    def test_codex_exec_command(self):
        self.assertEqual(classify.classify("exec_command", {"cmd": "rtk git log --oneline -20"}), ("git-read", "git-history"))
        self.assertEqual(classify.classify("exec_command", {"cmd": "rtk read codemap.md"}), ("read", "read-docs"))
        self.assertEqual(classify.classify("write_stdin", {"session_id": 1, "chars": ""}), ("run", "other"))
        self.assertEqual(classify.classify("update_plan", {"plan": []}), ("other", "other"))
        self.assertEqual(classify.classify("spawn_agent", {}), ("other", "other"))

    def test_search_need(self):
        self.assertEqual(classify.search_need("pub fn resolve"), "find-definition")
        self.assertEqual(classify.search_need("^export (const|function) useAuth"), "find-definition")
        self.assertEqual(classify.search_need("def parse_args"), "find-definition")
        self.assertEqual(classify.search_need("struct\\s+Config"), "find-definition")
        self.assertEqual(classify.search_need("UserService"), "find-usages")
        self.assertEqual(classify.search_need("\\bparse_args\\b"), "find-usages")
        self.assertEqual(classify.search_need("parse_args("), "find-usages")
        self.assertEqual(classify.search_need("timeout"), "search-string")
        self.assertEqual(classify.search_need("could not open file"), "search-string")
        self.assertEqual(classify.search_need("foo.*bar"), "search-regex")
        self.assertEqual(classify.search_need("^\\s*#\\[test\\]"), "search-regex")
        self.assertEqual(classify.search_need("a.b", fixed=True), "search-string")


if __name__ == "__main__":
    unittest.main()
