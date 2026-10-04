#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Tests for literal-call extraction and private native-command receipts."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('observed', Path(__file__).with_name('bench-plan-observed-corpus.py'))
observed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observed)
RECORDER = Path(__file__).with_name('record-plan-request.py').resolve()
record_spec = importlib.util.spec_from_file_location('recorder', RECORDER)
recorder = importlib.util.module_from_spec(record_spec)
record_spec.loader.exec_module(recorder)


def tool(command):
    return '\u22eetool bash ' + json.dumps({'command': command})


class ExtractionContract(unittest.TestCase):
    def test_literal_quoted_prompt_and_pipeline_flags(self):
        calls, _ = observed.calls(tool('PIXEL_METRICS=0 pixel plan "locate parser; explain errors"; pixel plan --query hotspots . 2>&1 | head -60'))
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0]['prompt'], 'locate parser; explain errors')
        self.assertIsNone(calls[1]['prompt'])
        self.assertEqual(calls[1]['explicit_query'], 'hotspots')
        self.assertEqual(calls[1]['argv'], ['pixel', 'plan', '--query', 'hotspots', '.'])

    def test_mentions_help_and_code_generation_are_not_calls(self):
        for text in ['Try pixel plan "unused functions"', tool('echo "pixel plan unused functions"'),
                     tool('pixel plan --help'), tool("python3 - <<'PY'\nprint('pixel plan unused functions')\nPY")]:
            self.assertEqual(observed.calls(text)[0], [])

    def test_dynamic_prompt_or_changed_cwd_is_not_guessed(self):
        for text in [tool('pixel plan "$TASK"'), tool('cd elsewhere; pixel plan "locate parser"')]:
            calls, reasons = observed.calls(text)
            self.assertEqual(calls, [])
            self.assertTrue(reasons)

    def test_explicit_query_is_not_fabricated_into_natural_language(self):
        calls, _ = observed.calls(tool('pixel plan --query dead-code'))
        self.assertIsNone(calls[0]['prompt'])
        self.assertEqual(calls[0]['explicit_query'], 'dead-code')

    def test_action_match_requires_same_repository_and_success(self):
        call = {'argv': ['pixel', 'plan', 'locate parser']}
        event = {'invocation_id': 'one', 'ts_ms': 1001, 'cwd': '/repo', 'args': 'plan locate parser', 'outcome': 'ok'}
        events = [event, dict(event, invocation_id='other-repo', cwd='/elsewhere'),
                  dict(event, invocation_id='failed', outcome='error'),
                  dict(event, invocation_id='late', ts_ms=31000)]
        self.assertEqual(observed.corroborating_actions(call, 1000, '/repo', events), ['one'])


FAKE = '''#!/usr/bin/env python3
import json,sys,time
from pathlib import Path
root=Path.cwd()
args=sys.argv[1:]
if args==['--version']:
 print('pixel fixture\\ncommit: fixture-only')
elif args[0]=='repo-state':
 if (root/'.bad-state').exists(): print('null');sys.exit(0)
 if (root/'.break-state').exists(): sys.exit(9)
 print(json.dumps({'head':('b' if (root/'.moved').exists() else 'a')*40,'branch':'fixture','dirty':[],'dirty_count':0,'root':str(root)}))
elif args[0]=='plan':
 count=root/'.calls'
 count.write_text(str(int(count.read_text())+1 if count.exists() else 1))
 prompt=args[1] if len(args)>1 else ''
 if prompt=='slow':
  print('partial',flush=True);time.sleep(2)
 if prompt=='move-head': (root/'.moved').touch()
 if prompt=='break-state': (root/'.break-state').touch()
 if prompt=='fail':
  sys.stdout.buffer.write(b'native failed stdout\\n')
  sys.stderr.buffer.write(b'native failed stderr\\n')
  sys.exit(7)
 sys.stdout.buffer.write(b'native stdout\\n')
 sys.stderr.buffer.write(b'native stderr\\n')
else:
 sys.exit(11)
'''


class CaptureContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root/'repo'
        self.repo.mkdir()
        self.binary = self.root/'pixel-fixture'
        self.binary.write_text(FAKE)
        self.binary.chmod(0o700)
        self.output = self.root/'private'/'receipt.json'

    def invoke(self, prompt='locate parser', *extra):
        command = [sys.executable, str(RECORDER), '--pixel', str(self.binary), '--repo', str(self.repo),
                   '--output', str(self.output), '--purpose', 'mechanism-fixture', '--prompt', prompt, *extra]
        return subprocess.run(command, capture_output=True)

    def receipt(self):
        return json.loads(self.output.read_text())

    def test_success_preserves_bytes_and_pins_private_receipt(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b'native stdout\n')
        self.assertEqual(result.stderr, b'native stderr\n')
        record = self.receipt()
        self.assertTrue(record['plan_started'])
        self.assertEqual(record['purpose'], 'mechanism-fixture')
        self.assertEqual(record['capture_status'], 'complete')
        self.assertEqual(record['before']['head'], 'a'*40)
        self.assertTrue(record['before_after_equal'])
        self.assertTrue(record['launcher_unchanged'])
        self.assertEqual(record['completion']['stdout_sha256'], hashlib.sha256(result.stdout).hexdigest())
        self.assertNotIn('native stdout', self.output.read_text())
        self.assertEqual(stat.S_IMODE(self.output.stat().st_mode), 0o600)
        self.assertIn('not_attested', record['serving_backend'])

    def test_failure_preserves_native_exit_and_streams(self):
        result = self.invoke('fail')
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, b'native failed stdout\n')
        self.assertEqual(result.stderr, b'native failed stderr\n')
        self.assertEqual(self.receipt()['native_exit_code'], 7)

    def test_existing_receipt_refused_without_repeating_command(self):
        self.invoke()
        original = self.output.read_bytes()
        result = self.invoke()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.output.read_bytes(), original)
        self.assertEqual((self.repo/'.calls').read_text(), '1')

    def test_source_change_is_disclosed_not_hidden(self):
        result = self.invoke('move-head')
        self.assertEqual(result.returncode, 0)
        self.assertFalse(self.receipt()['before_after_equal'])
        self.assertEqual(self.receipt()['after']['head'], 'b'*40)

    def test_postflight_failure_keeps_native_exit_and_marks_incomplete(self):
        result = self.invoke('break-state')
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b'native stdout\n')
        self.assertIn(b'postflight capture failed', result.stderr)
        self.assertEqual(self.receipt()['capture_status'], 'incomplete_postflight')
        self.assertNotIn('after', self.receipt())

    def test_timeout_is_not_recorded_as_a_native_success(self):
        result = self.invoke('slow', '--timeout', '0.2')
        self.assertEqual(result.returncode, 124)
        self.assertTrue(self.receipt()['timed_out'])
        self.assertIsNone(self.receipt()['native_exit_code'])
        self.assertEqual(self.receipt()['completion']['stdout_sha256'], hashlib.sha256(result.stdout).hexdigest())

    def test_invalid_metadata_stops_before_native_plan(self):
        (self.repo/'.bad-state').touch()
        result = self.invoke()
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.repo/'.calls').exists())
        self.assertFalse(self.receipt()['plan_started'])
        self.assertEqual(self.receipt()['capture_status'], 'failed')
        self.assertIn('incomplete', self.receipt()['capture_error'])

    def test_receipt_write_failure_does_not_hide_native_output(self):
        class Stream(io.StringIO):
            def __init__(self):
                super().__init__()
                self.buffer = io.BytesIO()
        stdout, stderr = Stream(), Stream()
        args = SimpleNamespace(repo=self.repo, pixel=str(self.binary), output=self.output,
            prompt='locate parser', purpose='mechanism-fixture', query=None, tag=None,
            limit=None, format=None, max_todos=None, metrics=None, json=False, no_verify=False, timeout=60)
        with (patch.object(recorder.json, 'dump', side_effect=OSError('fixture disk failure')),
              patch.object(recorder.sys, 'stdout', stdout), patch.object(recorder.sys, 'stderr', stderr)):
            code = recorder.capture(args)
        self.assertEqual(code, 0)
        self.assertEqual(stdout.buffer.getvalue(), b'native stdout\n')
        self.assertEqual(stderr.buffer.getvalue(), b'native stderr\n')
        self.assertIn('no valid capture was saved', stderr.getvalue())
        self.assertEqual(self.output.read_bytes(), b'')

    def test_existing_symlink_receipt_cannot_redirect_writes(self):
        self.output.parent.mkdir()
        target = self.root/'must-not-exist'
        self.output.symlink_to(target)
        result = self.invoke()
        self.assertEqual(result.returncode, 2)
        self.assertFalse(target.exists())
        self.assertFalse((self.repo/'.calls').exists())


if __name__ == '__main__':
    unittest.main()
