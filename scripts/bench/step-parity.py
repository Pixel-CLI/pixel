#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Does the Rust step cover what its probe script covered? (item 6: probes become steps in
``crates/pixel/src/execution_brief/steps/``).

    step-parity.py <name> [--bin PIXEL] [--questions Q.jsonl] [--ids a,b] [--report-dir D]

Runs ``probe-test.py`` offline twice on the same questions with the same baseline briefs: once on the script
``probes/<name>/probe``, once on the step of the same name, then compares what each merged into every brief. The
step is driven through the ignored test ``steps::tests::print_the_output_of_the_step_named_by_the_environment`` of
the crate's unit-test binary, behind a throwaway probe directory that reuses the script's ``pixel-plugin.toml`` (so
``when``, ``budget_ms`` and ``mode`` are the manifest's, which ``steps::tests`` pins to the step's). ``--bin`` should
be a pixel built from the commit before the port: a binary that already runs the step would put its blocks in the
baseline brief of both arms. Prints each arm's verdict line, then ``identical`` / ``different`` per question and a
total; exits 1 on any difference or any failed arm. Standard library only; run with ``python3 -I``.
"""

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parents[1]
TEST = "execution_brief::steps::tests::print_the_output_of_the_step_named_by_the_environment"
SHIM = '''#!/usr/bin/env python3
import os, subprocess, sys
done = subprocess.run([{binary!r}, "--ignored", "--exact", {test!r}, "--nocapture", "--test-threads=1"],
                      input=sys.stdin.read(), capture_output=True, text=True, env=dict(os.environ, PIXEL_STEP={name!r}))
out = done.stdout
a, b = out.find("<<<STEP\\n"), out.rfind("STEP>>>")
if a < 0 or b < a:
    sys.stderr.write(done.stderr[-400:])
    sys.exit(1)
sys.stdout.write(out[a + 8:b])
'''


def test_binary():
    """The unit-test binary of the ``pixel-cli`` crate, built if it is not."""
    done = subprocess.run(["cargo", "test", "-p", "pixel-cli", "--bin", "pixel", "--no-run", "--message-format=json"],
                          cwd=WORKTREE, capture_output=True, text=True, check=True)
    for line in done.stdout.splitlines():
        message = json.loads(line) if line.startswith("{") else {}
        if message.get("executable") and message.get("profile", {}).get("test") and \
                message.get("target", {}).get("name") == "pixel":
            return message["executable"]
    sys.exit("step-parity: no unit-test binary of pixel-cli in cargo's output")


def load_probe_test():
    spec = importlib.util.spec_from_file_location("probe_test", HERE / "probe-test.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules["probe_test"] = module
    spec.loader.exec_module(module)
    return module


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("name")
    parser.add_argument("--bin", default=str(WORKTREE / "target/dev-release/pixel"))
    parser.add_argument("--questions", default=str(WORKTREE / "eval/brief-bench/questions.jsonl"))
    parser.add_argument("--ids", default=None)
    parser.add_argument("--report-dir", default=None)
    args = parser.parse_args(argv)
    script = WORKTREE / "probes" / args.name
    if not (script / "probe").is_file():
        sys.exit(f"step-parity: no probes/{args.name}/probe to compare with")
    shim_root = Path(tempfile.mkdtemp(prefix="step-parity-"))
    shim = shim_root / args.name
    shim.mkdir()
    shutil.copy(script / "pixel-plugin.toml", shim / "pixel-plugin.toml")
    (shim / "probe").write_text(SHIM.format(binary=test_binary(), test=TEST, name=args.name))
    (shim / "probe").chmod(0o755)
    probe_test = load_probe_test()
    captured = []
    evaluate = probe_test.evaluate

    def spy(*spied, **named):
        rows = evaluate(*spied, **named)
        captured.append(rows)
        return rows

    probe_test.evaluate = spy
    status = 0
    for label, directory in (("script", script), ("step", shim)):
        argv = [str(directory), "--questions", args.questions, "--bin", args.bin]
        if args.ids:
            argv += ["--ids", args.ids]
        if args.report_dir:
            argv += ["--report-dir", str(Path(args.report_dir) / label)]
        print(f"[{label}]")
        status |= probe_test.main(argv)
    if len(captured) != 2:
        sys.exit("step-parity: an arm did not evaluate")
    same = differ = 0
    for before, after in zip(*captured):
        verdict = "identical" if before["text"] == after["text"] else "different"
        if before["ran"] or after["ran"]:
            print(f"  {before['id']}: {verdict} (script {before['probe_ms']} ms, step {after['probe_ms']} ms, "
                  f"calls {before['before']['calls']} -> {before['after']['calls']} / {after['after']['calls']})")
        same += verdict == "identical"
        differ += verdict == "different"
    print(f"PARITY {args.name} | {same} identical, {differ} different of {same + differ} questions")
    shutil.rmtree(shim_root, ignore_errors=True)
    return 1 if differ or status else 0


if __name__ == "__main__":
    sys.exit(main())
