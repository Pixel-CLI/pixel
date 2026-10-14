#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Build eval/brief-gate/prompts-multirepo.jsonl: the labelled set the brief gate is refitted on (#906).

The first gate was fitted on prompts about Pixel alone. This set adds the three other repositories of the
tool census and the census prompts themselves:

* on-topic: the 20 census prompts (manifest.json of scripts/tool-census, four repositories) plus 30 plain-English
  prompts written for each of excalidraw, requests and ripgrep at their pinned commits;
* off-topic: 40 prompts (chat, git/release/CI operations, generic programming, other applications' bugs and
  tasks) with `repo` "any": they are scored against every repository, since they are off topic for all.

`split` is dealt alternately by the SHA-256 of the text inside each (repo, on_topic) stratum, so each stratum is
within one row of even.

usage: multirepo_set.py <manifest.json> <out.jsonl>
"""
import hashlib
import json
import sys

SHAS = {
    "pixel": "85bede7d9a3c4e385e0a2045241a6466efbd8cbd",
    "excalidraw": "541653c7c9d587d463a47daacff9fb30e45a52e7",
    "requests": "611c6162cbc4ac2020a2f91c7cfa4f3abf9bbb60",
    "ripgrep": "3fce3b5bb0236da2df6d99672afb8a719642eca7",
}

ON_TOPIC = {
    "requests": [
        "how does requests decide which proxy to use for a url",
        "where are cookies stored on a session and how do they get sent with the next request",
        "why does a POST turn into a GET after a 302 redirect",
        "what happens when the server sends a gzip response body, who decodes it",
        "I want to add a custom transport adapter that retries failed connections, how do I mount it",
        "how is the User-Agent header built and can I change the default",
        "explain how timeouts are applied, is there a default timeout",
        "where does requests verify TLS certificates and how do I point it at my own CA bundle",
        "make iter_content raise a clearer error when the connection drops midway",
        "add a helper that parses the Retry-After header into seconds",
        "the json method blows up on an empty body, what exception is raised and where",
        "how does basic auth get encoded into the Authorization header",
        "which code handles the netrc file lookup",
        "when I upload files as multipart, where is the form encoded",
        "a relative redirect location with unicode characters breaks, find where redirects get resolved",
        "how are the status codes exposed as requests.codes attributes",
        "what is the difference between a session request and the top level request function in the code",
        "give me a rundown of the exception hierarchy",
        "where are hooks dispatched and when do they run",
        "how does the case insensitive dict keep the original key casing",
        "I'm getting an SSLError with a self-signed certificate, which code path raises it",
        "what does raise_for_status do and which exceptions come out of it",
        "how are query string parameters merged with the ones already in the url",
        "when streaming a large download, how does the response keep the connection open and when is it released",
        "check how digest authentication builds the nonce count and whether it is thread safe",
        "add type hints to the cookie jar helpers",
        "why are my custom headers dropped when a redirect goes to a different host",
        "where is the idna handling for international domain names",
        "list the places that read environment variables like the CA bundle one",
        "how does the library work out the encoding of a response when the server doesn't say",
    ],
    "ripgrep": [
        "how does ripgrep decide a file is binary and skip it",
        "where is the gitignore matching implemented",
        "why does rg not search hidden files by default and where is that decided",
        "what is the difference between the glob and iglob flags in the code",
        "how are multiple patterns combined into one regex",
        "I'd like a flag that prints the longest matching line, where would I add it",
        "how does the parallel directory walker split work between threads",
        "where does ripgrep detect that stdout is a terminal and turn on color",
        "how does it choose between memory maps and reading the file normally",
        "explain how line terminators and the crlf option are handled",
        "how does the searcher cope with extremely long lines",
        "what happens when a pattern is invalid, where is the error message produced",
        "how is the list of built-in file types stored",
        "how does the replace flag expand capture groups",
        "where is the JSON output printer",
        "why does searching a symlink loop not hang",
        "how does sorting by path work when the search runs in parallel",
        "how is context before and after a match printed",
        "how is PCRE2 selected as the engine when I pass the pcre flag",
        "what is the difference between ignoring vcs files and ignoring all ignore files",
        "make the default number of threads configurable through an environment variable",
        "find where encoding detection happens, the byte order mark and utf-16 handling",
        "how are the shell completions generated",
        "how does the multiline option change the matching strategy",
        "where is the man page text generated",
        "globs with a trailing slash don't match directories, find the bug",
        "how does ripgrep search compressed files",
        "how does it decide whether it can use a literal prefilter instead of the regex engine",
        "which module prints the heading, line numbers and columns in the standard printer",
        "how are the ignore rules from parent directories discovered",
    ],
    "excalidraw": [
        "how does the canvas decide which element is under the mouse when I click",
        "where is the undo and redo history stored and how are the deltas applied",
        "what happens when you paste an image from the clipboard",
        "I want to add a keyboard shortcut for toggling the grid, where do actions get registered",
        "explain how collaboration syncs elements between users and resolves conflicts",
        "where does the app save the scene to local storage and how often",
        "the eraser tool leaves a ghost of the deleted element sometimes, find where erasing is handled",
        "how does binding an arrow to a shape work",
        "make the default stroke width thicker for new rectangles",
        "how is the library of reusable shapes persisted",
        "where is the export to png implemented and how is the scale chosen",
        "how are fonts loaded and subset for svg export",
        "what decides whether text gets wrapped inside a container shape",
        "add a menu item to the main menu that opens the help dialog",
        "how is the hand drawn look generated, which library draws the rough strokes",
        "the zoom level resets when I reload, where is it stored",
        "how does frame clipping work for elements inside frames",
        "which component renders the context menu and how are its items built",
        "where is dark mode applied and how are colors inverted for the canvas",
        "walk me through how a freedraw stroke is recorded from pointer events",
        "how are element ids generated and are they stable across sessions",
        "I need to support a new language, how are translations loaded",
        "where is the code that snaps elements to each other while dragging",
        "why does text editing lose focus when I click on the toolbar",
        "how does the app decide between touch and mouse behaviour on a tablet",
        "add a maximum zoom limit option to the props of the editor component",
        "where are the scene's elements serialized to the file format",
        "how do I embed the editor in my own react app, what props does it take",
        "what triggers a re-render of the static canvas versus the interactive canvas",
        "find where the laser pointer trail is drawn",
    ],
}

OFF_TOPIC = {
    "chat": [
        "write me a short poem about autumn",
        "what's a good name for a golden retriever puppy",
        "summarize what we discussed so far in a few bullet points",
        "translate this sentence to spanish: where is the library",
        "I'm feeling stuck today, any advice on staying motivated",
        "what time zone is tokyo in",
        "give me a recipe for banana bread",
        "what's the weather going to be like tomorrow",
    ],
    "ops": [
        "rebase my branch on main and force push it",
        "cut a new release and tag it v2.3.0",
        "merge the open pull requests that are green",
        "why is the CI pipeline red on the last commit, rerun it",
        "delete the local branches that were already merged",
        "bump the version and publish to the package registry",
        "squash the last four commits into one",
        "cherry-pick the hotfix commit onto the release branch",
    ],
    "generic-code": [
        "how do I center a div horizontally and vertically",
        "what's the difference between a mutex and a semaphore",
        "write a regular expression that matches an email address",
        "explain big O notation with examples",
        "how do I reverse a linked list iteratively",
        "what is the best way to learn rust as a beginner",
        "how does garbage collection work in java",
        "give me a python one-liner to flatten a nested list",
        "what's the difference between tcp and udp",
        "explain how the oauth2 authorization code flow works",
        "what are the pros and cons of microservices",
        "how do I undo the last git commit but keep the changes",
    ],
    "other-repo": [
        "the login endpoint of our django app returns 500 since yesterday, find and fix it",
        "our kubernetes pod keeps crash looping after the last deploy",
        "the postgres migration fails with a duplicate column error",
        "add pagination to the orders table in the admin dashboard",
        "the mobile app crashes on startup when the user is offline",
        "the stripe webhook handler is not verifying signatures",
        "our terraform plan wants to destroy the load balancer, why",
        "make the checkout button blue and move it to the right",
        "the nightly ETL job times out when the dataset grows past a million rows",
        "the spring boot service leaks memory under load",
        "add dark mode to the settings page of our react native app",
        "how does the auth middleware decide between jwt and session",
    ],
}


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def deal(rows):
    """Alternate dev/test by text hash inside one stratum."""
    for index, row in enumerate(sorted(rows, key=lambda r: digest(r["text"]))):
        row["split"] = "dev" if index % 2 == 0 else "test"


def main(manifest, out):
    census = json.load(open(manifest))["prompts"]
    rows = []
    for item in census:
        rows.append({"text": item["prompt"], "on_topic": True, "repo": item["repo"], "sha": SHAS[item["repo"]],
                     "source": "census", "kind": "plain", "census_id": item["id"]})
    for repo, prompts in ON_TOPIC.items():
        for text in prompts:
            rows.append({"text": text, "on_topic": True, "repo": repo, "sha": SHAS[repo], "source": "written",
                         "kind": "plain"})
    for kind, prompts in OFF_TOPIC.items():
        for text in prompts:
            rows.append({"text": text, "on_topic": False, "repo": "any", "sha": None, "source": "written",
                         "kind": kind})
    strata = {}
    for row in rows:
        strata.setdefault((row["repo"], row["on_topic"]), []).append(row)
    for stratum in strata.values():
        deal(stratum)
    for number, row in enumerate(rows, 1):
        row["id"] = f"mr-{number:03d}"
        row["lang"] = "en"
    with open(out, "w") as handle:
        for row in rows:
            ordered = {key: row[key] for key in ("id", "text", "lang", "on_topic", "repo", "sha", "source", "kind", "split")}
            if "census_id" in row:
                ordered["census_id"] = row["census_id"]
            handle.write(json.dumps(ordered, ensure_ascii=False) + "\n")
    print(len(rows), "rows", {k: len(v) for k, v in strata.items()})


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
