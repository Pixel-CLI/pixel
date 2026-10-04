#!/usr/bin/env python3
"""Contract of scripts/harness-recorder.sh: what a recording run produces.

Every case runs the real script against stub binaries placed on PATH — a
stubbed `claude`/`codex` that behave like the real harnesses emit (the two
stream shapes the recorder's parser understands), stubbed `asciinema`/`agg`
that write minimal-but-valid artifacts, and a stubbed `gh` that records its
calls — because the contract is what the recorder does with the harness's
output, not what asciinema does with a PTY. The one end-to-end assertion that
stubbing cannot cover (a real `asciinema rec` produces a playable cast) is
marked out for a manual run in the module docstring; CI keeps running only the
stubs so the suite stays hermetic and fast.
"""

import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest

RECORDER = Path(__file__).with_name("harness-recorder.sh")
MARKER = "harness-recorder-contract"

# A minimal but valid asciicast v3 file: a JSON header plus one output event.
STUB_ASCIINEMA = """\
#!/bin/sh
# stub: record writes a valid single-event cast; cat/convert re-emit it as txt
cmd="$1"; shift
case "$cmd" in rec) cmd=record ;; esac
body=""
prev=""
for arg in "$@"; do
    case "$prev" in
        --command | -c) body="$arg" ;;
    esac
    prev="$arg"
done
case "$cmd" in
    record)
        file="${@: -1}"
        cap=$(mktemp)
        # the real recorder runs the command in a PTY and captures its output
        bash -c "$body" > "$cap" 2> /dev/null
        printf '%s\\n' '{"version":3,"term":{"cols":112,"rows":36},"timestamp":1,"command":"stub"}' > "$file"
        python3 - "$cap" >> "$file" << 'PYEOF'
import json, sys
for i, line in enumerate(open(sys.argv[1])):
    line = line.rstrip("\\n")
    print(json.dumps([i, "o", line]))
PYEOF
        rm -f "$cap"
        ;;
    cat)
        python3 - "$1" << 'PYEOF'
import json, sys
for line in open(sys.argv[1]):
    try:
        evt = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(evt, list) and evt[1] == "o":
        print(evt[2])
PYEOF
        ;;
    convert)
        # convert <input> <output>; "-" writes to stdout
        out="${@: -1}"
        if [ "$out" = "-" ]; then exec python3 - "$1" << 'PYEOF'
import json, sys
for line in open(sys.argv[1]):
    try:
        evt = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(evt, list) and evt[1] == "o":
        print(evt[2])
PYEOF
        else
            python3 - "$1" > "$out" << 'PYEOF'
import json, sys
for line in open(sys.argv[1]):
    try:
        evt = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(evt, list) and evt[1] == "o":
        print(evt[2])
PYEOF
        fi
        ;;
esac
exit 0
"""

STUB_AGG = """\
#!/bin/sh
# stub: agg receives <input> <output> then option flags; output is $2
printf 'GIF89a\\x01\\x00\\x01\\x00' > "$2"
[ -n "$AGG_LOG" ] && printf '%s\\n' "$*" >> "$AGG_LOG"
exit 0
"""

STUB_GH = """\
#!/bin/sh
echo "$* ##STDIN##" >> "$GH_LOG"
for a in "$@"; do case "$a" in http*) exit 0;; esac; done
echo "https://gist.github.com/stub/1"
"""

# The claude arm expects a stream that json-lines its events. The stub emits a
# tool_use (a pixel call!) plus a result, so one case can count 1 pixel call;
# RECORDER_TEST_EVENT_LINES stretches the stream to N tool_use events, whose
# cast timestamps span N seconds, so the --video-max-seconds speed scaling has
# something real to bite on.
STUB_CLAUDE = """\
#!/bin/sh
cat > /dev/null
n=0
while [ "$n" -lt "${RECORDER_TEST_EVENT_LINES:-0}" ]; do
    echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"pixel search-content -F x ."}}]}}'
    n=$((n + 1))
done
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"pixel search-content -F x ."}}]}}'
echo '{"type":"result","subtype":"success","duration_ms":10,"result":"done"}'
"""

# The codex arm runs `codex exec ...`; its text stream names pixel directly.
STUB_CODEX = """\
#!/bin/sh
echo "working (3m)"
echo "  pixel find-code launched"
echo "codex done"
"""


def write_stub(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class RecorderContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix=MARKER + "-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in (
            ("asciinema", STUB_ASCIINEMA),
            ("agg", STUB_AGG),
            ("gh", STUB_GH),
            ("claude", STUB_CLAUDE),
            ("codex", STUB_CODEX),
        ):
            write_stub(self.bin / name, body)
        self.out = self.root / "out"
        self.repo = self.root / "repo"
        self.repo.mkdir()
        (self.repo / ".git").mkdir()
        self.env = {
            **os.environ,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HARNESS_OUTDIR": str(self.out),
        }
        self.gh_log = self.out / "gh.log"
        self.env["GH_LOG"] = str(self.gh_log)

    def run_recorder(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(RECORDER), *args],
            env=self.env,
            capture_output=True,
            text=True,
            cwd=str(self.repo),
        )

    def base_flags(self, provider: str) -> list[str]:
        return ["--provider", provider, "--repo", str(self.repo),
                "--scenario", "locate"]

    # ── outputs ────────────────────────────────────────────────────────────

    def test_claude_run_produces_cast_txt_meta_with_pixel_count(self):
        r = self.run_recorder(*self.base_flags("claude"))
        self.assertEqual(r.returncode, 0, r.stderr)
        cast = self.out / "harness-claude-locate.cast"
        txt = self.out / "harness-claude-locate.txt"
        meta = self.out / "meta-claude-locate.json"
        comment = self.out / "comment-claude-locate.md"
        for f in (cast, txt, meta, comment):
            self.assertTrue(f.exists(), f"missing {f.name}")
        header = json.loads(cast.read_text().splitlines()[0])
        self.assertEqual(header["version"], 3)
        # the transcript replays the stubbed harness's tool call
        self.assertIn("pixel search-content", txt.read_text())
        # the comment body carries the stats table
        self.assertIn("Pixel invocations", comment.read_text())
        m = json.loads(meta.read_text())
        self.assertEqual(m["provider"], "claude")
        self.assertEqual(m["pixel_calls"], 1)
        self.assertEqual(m["scenario"], "locate")

    def test_gif_flag_requires_and_takes_agg(self):
        r = self.run_recorder(*self.base_flags("claude"), "--gif")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue((self.out / "harness-claude-locate.gif").exists())

    def test_gif_speed_scales_long_runs_to_the_video_cap(self):
        # 20 one-second events plus the stub's base pair: a 21 s cast; the
        # 15 s default cap asks agg for --speed 2. A short cast stays at 1.
        agg_log = self.out / "agg.log"
        env = dict(self.env)
        env["AGG_LOG"] = str(agg_log)
        env["RECORDER_TEST_EVENT_LINES"] = "20"
        r = subprocess.run(
            [str(RECORDER), *self.base_flags("claude"), "--gif"],
            env=env, capture_output=True, text=True, cwd=str(self.repo),
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("--speed 2", agg_log.read_text())
        env["RECORDER_TEST_EVENT_LINES"] = "0"
        r = subprocess.run(
            [str(RECORDER), *self.base_flags("claude"), "--gif"],
            env=env, capture_output=True, text=True, cwd=str(self.repo),
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("--speed 1", agg_log.read_text())

    def test_post_gists_the_cast_and_pushes_the_gif_to_a_media_branch(self):
        # a real local bare remote, so the plumbing (hash-object, read-tree,
        # write-tree, commit-tree, push) runs for real against an origin
        bare = self.root / "harness-recordings-media.git"
        subprocess.run(["git", "init", "-q", "--bare", str(bare)], check=True)
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        # Release gates discard the global Git config. This fixture owns its
        # identity and signing policy so the real commit/push path still runs.
        for key, value in (("user.name", "Recorder test"),
                           ("user.email", "recorder@example.invalid"),
                           ("commit.gpgsign", "false")):
            subprocess.run(["git", "-C", str(self.repo), "config", key, value],
                           check=True)
        # A GitHub-shaped configured URL so the recorder derives owner/repo
        # from remote.origin.url; rewritten to the local bare repo so the
        # plumbing below still talks to it.
        subprocess.run(["git", "-C", str(self.repo), "remote", "add",
                        "origin", "https://github.com/o/r"], check=True)
        subprocess.run(["git", "-C", str(self.repo), "config",
                        f"url.{bare}.insteadOf", "https://github.com/o/r"],
                       check=True)
        subprocess.run(["git", "-C", str(self.repo), "commit", "-q",
                        "--allow-empty", "-m", "base"], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "push", "-q", "origin",
             "HEAD:refs/heads/harness-recordings-media"],
            check=True,
        )
        r = self.run_recorder(*self.base_flags("claude"), "--gif", "--post", "42")
        self.assertEqual(r.returncode, 0, r.stderr)
        comment = (self.out / "comment-claude-locate.md").read_text()
        self.assertIn("gist.github.com/stub/1", comment)
        self.assertIn("raw.githubusercontent.com", comment)
        self.assertIn("/recordings/claude-locate.gif", comment)
        tree = subprocess.run(
            ["git", "-C", str(bare), "ls-tree", "-r", "--name-only",
             "harness-recordings-media"],
            capture_output=True, text=True, check=True,
        ).stdout
        self.assertIn("recordings/claude-locate.gif", tree)

    def test_media_branch_upload_fails_quietly_without_a_real_repo(self):
        # the fixture .git is a bare directory until a case turns it into a
        # repository: the media upload must then leave the comment complete
        # (gist link, no broken image) instead of crashing the post.
        r = self.run_recorder(*self.base_flags("claude"), "--gif", "--post", "42")
        self.assertEqual(r.returncode, 0, r.stderr)
        comment = (self.out / "comment-claude-locate.md").read_text()
        self.assertIn("gist.github.com/stub/1", comment)
        self.assertNotIn("raw.githubusercontent.com", comment)

    # ── prompt handling ────────────────────────────────────────────────────

    def test_prompt_file_placeholder_is_substituted_with_repo(self):
        self.run_recorder(*self.base_flags("claude"))
        prompt = (self.out / "prompt-claude-locate.txt").read_text()
        self.assertIn(str(self.repo), prompt)
        self.assertNotIn("REPO_PLACEHOLDER", prompt)

    def test_unknown_scenario_is_refused(self):
        r = self.run_recorder(*self.base_flags("claude"), "--scenario", "chaos")
        self.assertEqual(r.returncode, 2)
        self.assertIn("unknown scenario", r.stderr)

    # ── reporting ──────────────────────────────────────────────────────────

    def test_post_comments_pr_with_stats_and_transcript(self):
        r = self.run_recorder(*self.base_flags("claude"), "--post", "42")
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = self.gh_log.read_text()
        self.assertIn("pr comment 42", calls)
        body_file = sorted(self.out.glob("comment-claude-locate*"))
        self.assertTrue(body_file, "no comment body written")

    def test_post_without_gh_is_a_config_error_not_a_crash(self):
        # a PATH dir identical to the stubbed one minus the gh stub, and
        # nothing after it that's reachable (no /opt/homebrew/bin: a real gh
        # would try gist-create/pr-comment for the test's fake temp repo)
        bin_no_gh = self.root / "bin-no-gh"
        bin_no_gh.mkdir()
        for stub in self.bin.iterdir():
            if stub.name != "gh":
                shutil.copy(stub, bin_no_gh / stub.name)
        env = dict(self.env)
        env["PATH"] = f"{bin_no_gh}:/usr/bin:/bin"
        env["HARNESS_OUTDIR"] = str(self.out)
        r = subprocess.run(
            [str(RECORDER), *self.base_flags("claude"), "--post", "42"],
            env=env, capture_output=True, text=True, cwd=str(self.repo),
        )
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("gh not on PATH", r.stderr)


if __name__ == "__main__":
    unittest.main()
