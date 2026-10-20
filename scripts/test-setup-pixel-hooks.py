#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Execute installed Claude and Codex hooks in a disposable home, without an LLM."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bin', required=True)
parser.add_argument('--agent', choices=['all', 'claude', 'codex', 'claude,codex'], default='claude')
args = parser.parse_args()
binary = str(Path(args.bin).resolve())
installer = Path(__file__).resolve().parents[1] / '.github/actions/setup-pixel/install-hooks.sh'
with tempfile.TemporaryDirectory(prefix='pixel-hook-smoke-') as tmp:
    root = Path(tmp)
    home, repo = root / 'home', root / 'repo'
    home.mkdir()
    repo.mkdir()
    env = dict(os.environ, HOME=str(home), CODEX_HOME=str(home / '.codex'),
               XDG_CONFIG_HOME=str(home / '.config'),
               PI_CODING_AGENT_DIR=str(home / '.pi/agent'),
               PIXEL_DAEMON_AUTO_START='0', PIXEL_METRICS='off',
               PIXEL_ACTION_BIN=binary, PIXEL_ACTION_HOOKS_AGENT=args.agent,
               RUNNER_TEMP=tmp, GIT_CONFIG_GLOBAL='/dev/null')

    def run(command, payload=None):
        return subprocess.run(command, cwd=repo, env=env, input=payload,
                              text=True, capture_output=True, check=True, timeout=120).stdout

    run(['git', 'init'])
    (repo / 'hello.py').write_text('def greet():\n    return "hello"\n')
    run([binary, 'prepare-repo', str(repo), '--no-daemon', '--metrics', 'off', '--json'])
    settings = home / '.claude/settings.json'
    settings.parent.mkdir()
    settings.write_text(json.dumps({'model': 'keep-me'}))
    foreign = [home / p for p in ['.codex', '.gemini', '.pi/agent', '.config/opencode']]
    for directory in foreign:
        directory.mkdir(parents=True, exist_ok=True)
        (directory / 'settings.json').write_text('{"keep":true}')
    run(['bash', str(installer)])
    codex_settings = home / '.codex/hooks.json'
    codex_first = codex_settings.read_text() if codex_settings.exists() else None
    first = json.loads(settings.read_text())
    run(['bash', str(installer)])
    second = json.loads(settings.read_text())
    assert codex_first == (codex_settings.read_text() if codex_settings.exists() else None), 'Codex reinstall changed hooks'
    assert first == second, 'reinstall duplicated or changed hooks'
    assert second['model'] == 'keep-me', 'foreign Claude settings lost'
    selected = ['claude', 'codex'] if args.agent == 'all' else args.agent.split(',')
    if args.agent != 'all':
        if 'claude' not in selected:
            assert first == {'model': 'keep-me'}, 'unselected Claude modified'
        for directory in foreign:
            if directory == home / '.codex' and 'codex' in selected:
                continue
            assert list(directory.iterdir()) == [directory / 'settings.json'], directory
            assert (directory / 'settings.json').read_text() == '{"keep":true}'
    marker = repo / '.pixel/task-hook-observations.json'
    for provider in selected:
        configuration = second if provider == 'claude' else json.loads(codex_settings.read_text())
        for event, observed in [('SessionStart', 'session-start'), ('UserPromptSubmit', 'prompt-submit')]:
            commands = [hook['command'] for group in configuration['hooks'][event]
                        for hook in group['hooks'] if 'run-hook task-event' in hook.get('command', '')]
            assert commands, f'no installed {event} command'
            if marker.exists():
                marker.unlink()
            outputs = [run(['bash', '-c', command], json.dumps({
                'cwd': str(repo), 'session_id': 'ci-hook-smoke', 'hook_event_name': event,
                'prompt': 'Where is greet defined? Explain hello.py.'})) for command in commands]
            # Exit 0 alone is insufficient: hooks intentionally fail open.
            assert marker.exists(), f'{event} exited without executing the task bridge'
            receipt = json.loads(marker.read_text())[provider]
            assert receipt['event'] == observed, receipt
            assert receipt['session_id'] == 'ci-hook-smoke', receipt
            assert receipt['observed_unix'] > 0, receipt
            if event == 'UserPromptSubmit':
                responses = [json.loads(output) for output in outputs if output.strip()]
                assert any('[PIXEL:BRIEF]' in response.get('hookSpecificOutput', {}).get('additionalContext', '')
                           for response in responses), outputs
            print(f'PASS {args.agent}/{provider}: installed {event} executed')
print('PASS hook setup, preservation, idempotence and execution')
