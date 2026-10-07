#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Companion to the *-oauth-app.json example flows: capture + check OAuth credentials.

WHY THIS EXISTS
  The github/atlassian/notion flows drive the provider console with pixel-flow until the
  client secret is visible on screen, then END. pixel-flow cannot do the rest: `eval`
  output is discarded by `pixel flow run`, a flow cannot write files, read env vars or
  call an HTTP API. This script does exactly that, and nothing else:

    oauth-env.py capture <github|atlassian|notion|google>
        Reads client id + secret from the page the flow left open (one `agent-browser
        --session comet eval`, script on stdin), validates their shape, and writes
            export OAUTH_<PROVIDER>_CLIENT_ID=...
            export OAUTH_<PROVIDER>_CLIENT_SECRET=...
        to the file named by $OAUTH_ENV_OUT with mode 600. Other lines in that file are
        kept; only this provider's two keys are replaced.

    oauth-env.py check <github|atlassian|notion|google>
        Sanity-checks the stored credentials against the provider (see per-provider
        notes below) and prints a verdict.

    oauth-env.py store <provider>      (used by `capture`; reads eval output on stdin)

SECRET HYGIENE
  Secret values are never printed, logged or put on a command line (the eval script goes
  to agent-browser on stdin; HTTP checks use Python's urllib, not curl -u). Output is
  limited to lengths, "captured: true" and verdicts. Failure messages never include values.

AFTER THIS (manual follow-up, deliberately not automated)
  Your backend/app reads OAUTH_<PROVIDER>_CLIENT_ID / OAUTH_<PROVIDER>_CLIENT_SECRET from
  its PROCESS ENVIRONMENT. `source "$OAUTH_ENV_OUT"` in the shell that starts the API.
  Never commit that file.

CHECK DETAILS
  github     POST https://api.github.com/applications/<id>/token with HTTP basic auth
             id:secret and a dummy access_token. 404 = credentials valid, 401 = invalid.
  atlassian  POST https://auth.atlassian.com/oauth/token with a bogus authorization code.
             Any error other than invalid_client (typically invalid_grant) = client
             credentials accepted.
  google     POST https://oauth2.googleapis.com/token (form-encoded) with a bogus code.
             Any error other than invalid_client (typically invalid_grant) = accepted.
  notion     No check defined: none was established in the recorded session.

ENV
  OAUTH_ENV_OUT    required for capture/store/check: file to write/read (mode 600).
  OAUTH_CALLBACK_URL  required for the atlassian/google check: the callback_url you gave the flow
                   (sent as redirect_uri with the bogus code).
  AGENT_BROWSER_SESSION  optional, default "comet" (capture only).
"""

import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request

PROVIDERS = ("github", "atlassian", "notion", "google")

# HOW TO ADD A PROVIDER: (1) add its name to PROVIDERS, (2) add a page-capture script to
# CAPTURE_JS returning JSON.stringify({id, secret}), (3) add its (id, secret) shapes to
# SHAPES, (4) optionally add a branch to check(). Env vars are OAUTH_<NAME>_CLIENT_ID/_SECRET.


# Scripts run in the page the flow left open. Each returns JSON.stringify({id, secret}).
# Wrapped in IIFEs: the page context keeps top-level const/let between evals.
CAPTURE_JS = {
    # id: 20 alphanumerics after the "Client ID" label. secret: the FIRST 40-hex string
    # in the page HTML (shown once, right after "Generate a new client secret").
    # PITFALL: another 40-hex string earlier in the HTML would be picked instead; `check`
    # catches that (401).
    "github": r"""(()=>{
const m=document.body.innerText.match(/Client ID\s+([A-Za-z0-9]{20})(?![A-Za-z0-9])/);
const h=document.documentElement.innerHTML.match(/(?<![0-9a-f])[0-9a-f]{40}(?![0-9a-f])/);
return JSON.stringify({id:m?m[1]:null,secret:h?h[0]:null})})()""",
    # Settings page: "Client ID" is a textbox value; "Secret" is a masked password input
    # whose .value holds the real secret (the accessibility snapshot never shows it).
    "atlassian": r"""(()=>{
const inputs=[...document.querySelectorAll('input')];
const lab=i=>[...(i.labels||[])].map(l=>l.innerText).join(' ')+' '+(i.getAttribute('aria-label')||'')+' '+(i.name||'')+' '+(i.id||'');
const pw=inputs.find(i=>i.type==='password'&&i.value);
const idIn=inputs.find(i=>i.type!=='password'&&/client.?id/i.test(lab(i))&&i.value);
return JSON.stringify({id:idIn?idIn.value:null,secret:pw?pw.value:null})})()""",
    # id: the connection uuid in the URL. secret: inside the one-time dialog, as text or
    # as an input value; prefix secret_ or ntn_.
    "notion": r"""(()=>{
const idm=location.pathname.match(/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/i);
const d=document.querySelector('[role=dialog],[role=alertdialog]');
const txt=d?d.innerText+' '+[...d.querySelectorAll('input,textarea')].map(i=>i.value).join(' '):'';
const sm=txt.match(/((?:secret_|ntn_)[A-Za-z0-9_-]+)/);
return JSON.stringify({id:idm?idm[1]:null,secret:sm?sm[1]:null})})()""",
    # "OAuth client created" dialog: id and secret are in the dialog text or its inputs.
    # NOTE: newer consoles may NOT let you view the secret again after the dialog closes,
    # so capture before closing it (and never click "Download JSON").
    "google": r"""(()=>{
const d=document.querySelector('[role=dialog],[role=alertdialog],mat-dialog-container')||document.body;
const txt=d.innerText+' '+[...d.querySelectorAll('input,textarea')].map(i=>i.value).join(' ');
const im=txt.match(/\d+-[a-z0-9]+\.apps\.googleusercontent\.com/);
const sm=txt.match(/GOCSPX-[A-Za-z0-9_-]+/);
return JSON.stringify({id:im?im[0]:null,secret:sm?sm[0]:null})})()""",
}

SHAPES = {
    "github": (re.compile(r"[A-Za-z0-9]{20}"), re.compile(r"[0-9a-f]{40}")),
    "atlassian": (re.compile(r"[A-Za-z0-9]{20,64}"), re.compile(r"\S{20,}")),
    "notion": (
        re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", re.I),
        re.compile(r"(secret_|ntn_)[A-Za-z0-9_-]+"),
    ),
    "google": (
        re.compile(r"\d+-[a-z0-9]+\.apps\.googleusercontent\.com"),
        re.compile(r"GOCSPX-[A-Za-z0-9_-]+"),
    ),
}


def die(msg, code=1):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


def env_out():
    path = os.environ.get("OAUTH_ENV_OUT", "")
    if not path:
        die("OAUTH_ENV_OUT is not set (path of the env file to write/read)", 2)
    return path


def unwrap(value):
    """agent-browser prints the eval result as JSON (possibly string-in-string or inside
    an envelope). Peel it down to {"id":..., "secret":...}."""
    for _ in range(8):
        if isinstance(value, str):
            try:
                value = json.loads(value.strip())
                continue
            except ValueError:
                return None
        if isinstance(value, dict):
            if "id" in value and "secret" in value:
                return value
            for key in ("data", "result", "value"):
                if key in value:
                    value = value[key]
                    break
            else:
                return None
            continue
        return None
    return None


def store(provider, raw):
    pair = unwrap(raw)
    if pair is None:
        die("could not parse the page capture (is the flow's last page still open?)", 2)
    cid, secret = pair.get("id"), pair.get("secret")
    id_re, secret_re = SHAPES[provider]
    problems = []
    if not isinstance(cid, str) or not id_re.fullmatch(cid):
        problems.append("client id not found on the page or unexpected shape")
    if not isinstance(secret, str) or not secret_re.fullmatch(secret):
        problems.append("client secret not found on the page or unexpected shape")
    if problems:
        die("; ".join(problems) + " (values not shown)", 2)

    path = env_out()
    key = provider.upper()
    id_key, secret_key = f"OAUTH_{key}_CLIENT_ID", f"OAUTH_{key}_CLIENT_SECRET"
    kept = []
    if os.path.exists(path):
        with open(path) as fh:
            for line in fh:
                if re.match(rf"\s*(export\s+)?({id_key}|{secret_key})=", line):
                    continue
                kept.append(line if line.endswith("\n") else line + "\n")
    kept.append(f"export {id_key}={shlex.quote(cid)}\n")
    kept.append(f"export {secret_key}={shlex.quote(secret)}\n")

    directory = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(prefix=".oauth-env-", dir=directory)  # created mode 600
    try:
        with os.fdopen(fd, "w") as fh:
            fh.writelines(kept)
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise
    os.chmod(path, 0o600)

    print(
        f"captured: true  provider={provider}  client_id_len={len(cid)}  "
        f"client_secret_len={len(secret)}  file={path}  mode=600"
    )
    ignored = subprocess.run(
        ["git", "check-ignore", "-q", path],
        cwd=directory,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if ignored.returncode == 1:  # inside a git work tree and NOT ignored
        print("warning: this file is not git-ignored; do not commit it", file=sys.stderr)


def capture(provider):
    session = os.environ.get("AGENT_BROWSER_SESSION", "comet")
    try:
        proc = subprocess.run(
            ["agent-browser", "--session", session, "eval", "--stdin"],
            input=CAPTURE_JS[provider],
            capture_output=True,
            text=True,
            timeout=60,
        )
    except FileNotFoundError:
        die("agent-browser not found on PATH", 2)
    if proc.returncode != 0:
        die(f"agent-browser eval exited {proc.returncode}: {proc.stderr.strip()[:300]}", 2)
    store(provider, proc.stdout)


def load_creds(provider):
    key = provider.upper()
    want = {f"OAUTH_{key}_CLIENT_ID": None, f"OAUTH_{key}_CLIENT_SECRET": None}
    with open(env_out()) as fh:
        for line in fh:
            m = re.match(r"\s*(?:export\s+)?(\w+)=(.*)$", line.rstrip("\n"))
            if m and m.group(1) in want:
                parts = shlex.split(m.group(2))
                want[m.group(1)] = parts[0] if parts else ""
    cid, secret = want[f"OAUTH_{key}_CLIENT_ID"], want[f"OAUTH_{key}_CLIENT_SECRET"]
    if not cid or not secret:
        die(f"{provider} credentials not found in $OAUTH_ENV_OUT (run capture first)", 2)
    return cid, secret


def callback_url():
    url = os.environ.get("OAUTH_CALLBACK_URL", "")
    if not url:
        die("OAUTH_CALLBACK_URL is not set (use the callback_url you gave the flow)", 2)
    return url


def post(url, headers, body, basic=None, form=False):
    if form:
        data, ctype = urllib.parse.urlencode(body).encode(), "application/x-www-form-urlencoded"
    else:
        data, ctype = json.dumps(body).encode(), "application/json"
    req = urllib.request.Request(url, data=data, method="POST")
    req.add_header("User-Agent", "pixel-flow-oauth-env-check")
    req.add_header("Content-Type", ctype)
    for k, v in headers.items():
        req.add_header(k, v)
    if basic:
        import base64

        req.add_header("Authorization", "Basic " + base64.b64encode(f"{basic[0]}:{basic[1]}".encode()).decode())
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def check(provider):
    if provider == "notion":
        print("check: none defined for notion (no sanity check was established); nothing verified")
        return
    cid, secret = load_creds(provider)
    if provider == "github":
        status, _ = post(
            f"https://api.github.com/applications/{cid}/token",
            {"Accept": "application/vnd.github+json"},
            {"access_token": "x"},
            basic=(cid, secret),
        )
        if status == 404:
            print("check: github credentials VALID (HTTP 404 for the dummy token, as expected)")
        elif status == 401:
            print("check: github credentials INVALID (HTTP 401)")
            sys.exit(1)
        else:
            print(f"check: github unexpected HTTP {status}; inconclusive")
            sys.exit(1)
    elif provider == "google":
        status, body = post(
            "https://oauth2.googleapis.com/token",
            {},
            {
                "grant_type": "authorization_code",
                "client_id": cid,
                "client_secret": secret,
                "code": "bogus-code-for-credential-check",
                "redirect_uri": callback_url(),
            },
            form=True,
        )
        try:
            error = json.loads(body).get("error", "")
        except ValueError:
            error = ""
        if error == "invalid_client":
            print(f"check: google credentials INVALID (HTTP {status}, error=invalid_client)")
            sys.exit(1)
        print(f"check: google client credentials ACCEPTED (HTTP {status}, error={error or 'none'}; invalid_grant is expected for the bogus code)")
    else:
        status, body = post(
            "https://auth.atlassian.com/oauth/token",
            {},
            {
                "grant_type": "authorization_code",
                "client_id": cid,
                "client_secret": secret,
                "code": "bogus-code-for-credential-check",
                "redirect_uri": callback_url(),
            },
        )
        try:
            error = json.loads(body).get("error", "")
        except ValueError:
            error = ""
        if error == "invalid_client":
            print(f"check: atlassian credentials INVALID (HTTP {status}, error=invalid_client)")
            sys.exit(1)
        print(f"check: atlassian client credentials ACCEPTED (HTTP {status}, error={error or 'none'}; invalid_grant is expected for the bogus code)")


def main(argv):
    if len(argv) != 3 or argv[1] not in ("capture", "check", "store") or argv[2] not in PROVIDERS:
        die(f"usage: {os.path.basename(argv[0])} <capture|check|store> <{'|'.join(PROVIDERS)}>", 64)
    command, provider = argv[1], argv[2]
    if command == "capture":
        capture(provider)
    elif command == "store":
        store(provider, sys.stdin.read())
    else:
        check(provider)


if __name__ == "__main__":
    main(sys.argv)
