// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The step on small repositories, with the script's own output as the
//! expectation: each `expected` is what `probes/error-origin/probe` printed
//! on the same files and question (the Python suite's cases, one per test),
//! then the helpers on their own.

use super::*;
use crate::execution_brief::steps::testing::{git, repo, run};
use std::path::Path;
use std::time::{Duration, Instant};

/// One case: the repository, the question and what the script printed.
struct Case {
    prompt: &'static str,
    files: &'static [(&'static str, &'static str)],
    expected: &'static str,
}

fn ask(case: &Case) -> String {
    let dir = repo(case.files);
    run(&STEP, dir.path(), case.prompt, "")
}

const DESCRIBED_BEATS_MANUAL: Case = Case {
    prompt: "where does the message about no files being found come from and when is it printed",
    files: &[
        ("src/main.rs", r#"fn eprint_nothing_found() {
    err_message!(
        "No files were found, which means a filter \
         ran.\n\
         Running with --debug shows why files are being skipped."
    );
}

pub fn run(found: usize) {
    if found == 0 {
        eprint_nothing_found();
    }
}
"#),
        ("src/flags.rs", r#"pub fn doc_long() -> &'static str {
    r"
Print each file that would be found, without searching it.
This shows whether a file is being found or not.
"
}
"#),
    ],
    expected: r#"### error origin — described occurrence candidate; emission unverified at src/main.rs:3 near eprint_nothing_found (scope unresolved)
### src/main.rs:1-9 — candidate source occurrence; inspect the evidence category above
1| fn eprint_nothing_found() {
2|     err_message!(
3|         "No files were found, which means a filter \
4|          ran.\n\
5|          Running with --debug shows why files are being skipped."
6|     );
7| }
8| 
9| pub fn run(found: usize) {
### src/main.rs:9-13 — run → eprint_nothing_found: lexical call candidate; receiver/binding unresolved; scope unverified
9| pub fn run(found: usize) {
10|     if found == 0 {
11|         eprint_nothing_found();
12|     }
13| }
"#,
};

#[test]
fn described_beats_manual_prints_what_the_script_prints() {
    assert_eq!(ask(&DESCRIBED_BEATS_MANUAL), DESCRIBED_BEATS_MANUAL.expected);
}

const PYTHON_TRIPLE_QUOTES: Case = Case {
    prompt: "where does the runtime error about working outside of the request scope come from",
    files: &[
        ("pkg/globals.py", r#"_no_scope_msg = """\
Working outside of the request scope.

Open one with scope().
"""


def current():
    raise RuntimeError(_no_scope_msg)
"#),
        ("pkg/other.py", r#"def scope():
    return 'a request scope'
"#),
    ],
    expected: r#"### error origin — message definition; emission unverified at pkg/globals.py:2
### pkg/globals.py:1-8 — candidate source occurrence; inspect the evidence category above
1| _no_scope_msg = """\
2| Working outside of the request scope.
3| 
4| Open one with scope().
5| """
6| 
7| 
8| def current():
"#,
};

#[test]
fn python_triple_quotes_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_TRIPLE_QUOTES), PYTHON_TRIPLE_QUOTES.expected);
}

const CATALOGUE: Case = Case {
    prompt: "where is the alert text for a file that could not be loaded defined",
    files: &[
        ("locales/en.json", r#"{
  "alerts": {
    "invalidDrop": "Couldn't load the invalid file"
  }
}
"#),
        ("src/App.tsx", r#"function onDrop() {
  setAlert(t("alerts.invalidDrop"));
}
"#),
    ],
    expected: r#"### error origin — described occurrence candidate; emission unverified at src/App.tsx:2 near onDrop (scope unresolved)
### locales/en.json:3-3 — message definition in catalogue; emission unverified
3|     "invalidDrop": "Couldn't load the invalid file"
### src/App.tsx:1-3 — candidate source occurrence; inspect the evidence category above
1| function onDrop() {
2|   setAlert(t("alerts.invalidDrop"));
3| }
"#,
};

#[test]
fn catalogue_prints_what_the_script_prints() {
    assert_eq!(ask(&CATALOGUE), CATALOGUE.expected);
}

const LONG_STRING_IS_A_MANUAL: Case = Case {
    prompt: "where does the message about no files being found come from",
    files: &[
        ("src/doc.rs", r#"pub fn doc() -> &'static str {
    r"
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
No files being found is printed here.
"
}
"#),
    ],
    expected: "",
};

#[test]
fn long_string_is_a_manual_prints_what_the_script_prints() {
    assert_eq!(ask(&LONG_STRING_IS_A_MANUAL), LONG_STRING_IS_A_MANUAL.expected);
}

const INLINE_TEST_MODULE: Case = Case {
    prompt: "where does the message about no files being found by the walker come from",
    files: &[
        ("src/walk.rs", r#"pub fn ok() {}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        assert_eq!(f(), "No files were found by the walker");
    }
}
"#),
    ],
    expected: "",
};

#[test]
fn inline_test_module_prints_what_the_script_prints() {
    assert_eq!(ask(&INLINE_TEST_MODULE), INLINE_TEST_MODULE.expected);
}

const PYTHON_DOCSTRING: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def explain():
    """The amber river is frozen."""
"#),
    ],
    expected: r#"### error origin — documentation occurrence; no emission proved at engine.py:2 in explain
### engine.py:1-2 — candidate source occurrence; inspect the evidence category above
1| def explain():
2|     """The amber river is frozen."""
"#,
};

#[test]
fn python_docstring_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_DOCSTRING), PYTHON_DOCSTRING.expected);
}

const PYTHON_DEFINITION: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"NOTICE = "The amber river is frozen"
"#),
    ],
    expected: r#"### error origin — message definition; emission unverified at engine.py:1
### engine.py:1-1 — candidate source occurrence; inspect the evidence category above
1| NOTICE = "The amber river is frozen"
"#,
};

#[test]
fn python_definition_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_DEFINITION), PYTHON_DEFINITION.expected);
}

const PYTHON_LOCAL_RAISE: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def fail():
    raise ValueError("The amber river is frozen")
"#),
    ],
    expected: r#"### error origin — local raise syntax; message origin candidate at engine.py:2 in fail
### engine.py:1-2 — candidate source occurrence; inspect the evidence category above
1| def fail():
2|     raise ValueError("The amber river is frozen")
"#,
};

#[test]
fn python_local_raise_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_LOCAL_RAISE), PYTHON_LOCAL_RAISE.expected);
}

const PYTHON_RETHROW_0: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    try:
        connect()
    except LinkDisconnected as exc:
        if retry_allowed():
            recover()
        raise GatewayError(exc)
"#),
    ],
    expected: r#"### engine.py:4-9 — conversion/rethrow candidate: matching caught type; runtime message origin unresolved
4|     try:
5|         connect()
6|     except LinkDisconnected as exc:
7|         if retry_allowed():
8|             recover()
9|         raise GatewayError(exc)
### engine.py:1-1 — dependency boundary candidate: matching import name; binding unresolved; external emission not inspected
1| from transport.errors import LinkDisconnected
### engine.py:9-9 — local raise/rethrow syntax in this handler; message identity unverified
9|         raise GatewayError(exc)
Call context unresolved: method names alone do not establish receiver identity.
"#,
};

#[test]
fn python_rethrow_0_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_RETHROW_0), PYTHON_RETHROW_0.expected);
}

const PYTHON_RETHROW_1: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    try:
        connect()
    except LinkDisconnected as exc:
        if retry_allowed():
            recover()
        raise
"#),
    ],
    expected: r#"### engine.py:4-9 — conversion/rethrow candidate: matching caught type; runtime message origin unresolved
4|     try:
5|         connect()
6|     except LinkDisconnected as exc:
7|         if retry_allowed():
8|             recover()
9|         raise
### engine.py:1-1 — dependency boundary candidate: matching import name; binding unresolved; external emission not inspected
1| from transport.errors import LinkDisconnected
### engine.py:9-9 — local raise/rethrow syntax in this handler; message identity unverified
9|         raise
Call context unresolved: method names alone do not establish receiver identity.
"#,
};

#[test]
fn python_rethrow_1_prints_what_the_script_prints() {
    assert_eq!(ask(&PYTHON_RETHROW_1), PYTHON_RETHROW_1.expected);
}

const HOMONYMOUS_RECEIVER: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def send():
    raise ValueError("The amber river is frozen")

class Reporter:
    def send(self):
        return "log only"

def unrelated(reporter):
    reporter.send()

def execute():
    send()
"#),
    ],
    expected: r#"### error origin — local raise syntax; message origin candidate at engine.py:2 in send
### engine.py:1-8 — candidate source occurrence; inspect the evidence category above
1| def send():
2|     raise ValueError("The amber river is frozen")
3| 
4| class Reporter:
5|     def send(self):
6|         return "log only"
7| 
8| def unrelated(reporter):
### engine.py:8-12 — unrelated → send: lexical call candidate; receiver/binding unresolved; scope unverified
8| def unrelated(reporter):
9|     reporter.send()
10| 
11| def execute():
12|     send()
### engine.py:11-12 — execute → send: lexical call candidate; receiver/binding unresolved; scope unverified
11| def execute():
12|     send()
"#,
};

#[test]
fn homonymous_receiver_prints_what_the_script_prints() {
    assert_eq!(ask(&HOMONYMOUS_RECEIVER), HOMONYMOUS_RECEIVER.expected);
}

const EMITTER_BEATS_HELP: Case = Case {
    prompt: "where is the error printed when a search path does not exist produced",
    files: &[
        ("src/cli.rs", r#"fn paths() {
    if filesystem::is_existing_directory(path) {
        normalize(path);
    } else {
        print_error!(
            "Search path {:?} is not a directory.",
            path
        );
    }
}
fn path_separator(search_path: &str) {
    println!("Search path separator is configured here: {}", search_path);
}
const HELP: &str = "Set path separator when printing file paths";
"#),
    ],
    expected: r#"### src/cli.rs:1-11 — error-emission candidate; matching terms and source condition, exact runtime cause unproven
1| fn paths() {
2|     if filesystem::is_existing_directory(path) {
3|         normalize(path);
4|     } else {
5|         print_error!(
6|             "Search path {:?} is not a directory.",
7|             path
8|         );
9|     }
10| }
11| fn path_separator(search_path: &str) {
"#,
};

#[test]
fn emitter_beats_help_prints_what_the_script_prints() {
    assert_eq!(ask(&EMITTER_BEATS_HELP), EMITTER_BEATS_HELP.expected);
}

const EMITTER_NEEDS_TWO_TERMS: Case = Case {
    prompt: "where is the error printed",
    files: &[
        ("src/cli.rs", r#"fn paths() {
    if filesystem::is_existing_directory(path) {
        normalize(path);
    } else {
        print_error!(
            "Search path {:?} is not a directory.",
            path
        );
    }
}
fn path_separator(search_path: &str) {
    println!("Search path separator is configured here: {}", search_path);
}
const HELP: &str = "Set path separator when printing file paths";
"#),
    ],
    expected: "",
};

#[test]
fn emitter_needs_two_terms_prints_what_the_script_prints() {
    assert_eq!(ask(&EMITTER_NEEDS_TWO_TERMS), EMITTER_NEEDS_TWO_TERMS.expected);
}

const OTHER_LITERAL_ERROR: Case = Case {
    prompt: "where does the error for a missing source path come from and when does it print",
    files: &[
        ("src/open.rs", r#"fn open(path: &Path) {
    if !path.exists() {
        eprintln!("Source path was not found: {}", path.display());
    }
}
"#),
    ],
    expected: r#"### src/open.rs:1-5 — error-emission candidate; matching terms and source condition, exact runtime cause unproven
1| fn open(path: &Path) {
2|     if !path.exists() {
3|         eprintln!("Source path was not found: {}", path.display());
4|     }
5| }
"#,
};

#[test]
fn other_literal_error_prints_what_the_script_prints() {
    assert_eq!(ask(&OTHER_LITERAL_ERROR), OTHER_LITERAL_ERROR.expected);
}

const SECOND_EMITTER: Case = Case {
    prompt: "where is the error emitted for a missing source path that was not found",
    files: &[
        ("src/open.rs", r#"fn warning() { eprintln!("Search path warning"); }
fn open(path: &Path) {
    if !path.exists() {
        eprintln!("Missing source path was not found: {}", path.display());
    }
}
"#),
    ],
    expected: r#"### src/open.rs:2-6 — error-emission candidate; matching terms and source condition, exact runtime cause unproven
2| fn open(path: &Path) {
3|     if !path.exists() {
4|         eprintln!("Missing source path was not found: {}", path.display());
5|     }
6| }
"#,
};

#[test]
fn second_emitter_prints_what_the_script_prints() {
    assert_eq!(ask(&SECOND_EMITTER), SECOND_EMITTER.expected);
}

const QUOTED_SURVIVES_DIAGNOSTIC: Case = Case {
    prompt: r#"where is "The source path is missing" emitted"#,
    files: &[
        ("src/open.rs", r#"fn unrelated() { eprintln!("Missing source path failed to load"); }
fn open(path: &Path) {
    if !path.exists() {
        eprintln!("The source path is missing");
    }
}
"#),
    ],
    expected: r#"### src/open.rs:1-1 — error-emission candidate; matching terms and source condition, exact runtime cause unproven
1| fn unrelated() { eprintln!("Missing source path failed to load"); }
### error origin — quoted occurrence candidate; emission unverified at src/open.rs:4 near open (scope unresolved)
### src/open.rs:2-6 — candidate source occurrence; inspect the evidence category above
2| fn open(path: &Path) {
3|     if !path.exists() {
4|         eprintln!("The source path is missing");
5|     }
6| }
"#,
};

#[test]
fn quoted_survives_diagnostic_prints_what_the_script_prints() {
    assert_eq!(ask(&QUOTED_SURVIVES_DIAGNOSTIC), QUOTED_SURVIVES_DIAGNOSTIC.expected);
}

const HELP_IS_NOT_EMISSION: Case = Case {
    prompt: "where is the error printed when a search path does not exist produced",
    files: &[
        ("src/help.rs", r#"const HELP: &str = "Search path does not exist in the help output";
"#),
    ],
    expected: r#"### error origin — described occurrence candidate; emission unverified at src/help.rs:1
### src/help.rs:1-1 — candidate source occurrence; inspect the evidence category above
1| const HELP: &str = "Search path does not exist in the help output";
"#,
};

#[test]
fn help_is_not_emission_prints_what_the_script_prints() {
    assert_eq!(ask(&HELP_IS_NOT_EMISSION), HELP_IS_NOT_EMISSION.expected);
}

const UNPARSEABLE_PYTHON: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def broken(:
    raise ValueError("The amber river is frozen")
"#),
    ],
    expected: r#"### error origin — occurrence candidate; emission unverified at engine.py:2
### engine.py:1-2 — candidate source occurrence; inspect the evidence category above
1| def broken(:
2|     raise ValueError("The amber river is frozen")
"#,
};

#[test]
fn unparseable_python_prints_what_the_script_prints() {
    assert_eq!(ask(&UNPARSEABLE_PYTHON), UNPARSEABLE_PYTHON.expected);
}

const GLOBAL_MESSAGE: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def unrelated():
    return None

NOTICE = "The amber river is frozen"
"#),
    ],
    expected: r#"### error origin — message definition; emission unverified at engine.py:4
### engine.py:1-4 — candidate source occurrence; inspect the evidence category above
1| def unrelated():
2|     return None
3| 
4| NOTICE = "The amber river is frozen"
"#,
};

#[test]
fn global_message_prints_what_the_script_prints() {
    assert_eq!(ask(&GLOBAL_MESSAGE), GLOBAL_MESSAGE.expected);
}

const NESTED_SCOPE_0: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    try:
        connect()
    except LinkDisconnected as exc:
        def later():
            raise GatewayError(exc)
        return None
"#),
    ],
    expected: "",
};

#[test]
fn nested_scope_0_prints_what_the_script_prints() {
    assert_eq!(ask(&NESTED_SCOPE_0), NESTED_SCOPE_0.expected);
}

const NESTED_SCOPE_1: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    try:
        connect()
    except LinkDisconnected as exc:
        async def later():
            raise GatewayError(exc)
        return None
"#),
    ],
    expected: "",
};

#[test]
fn nested_scope_1_prints_what_the_script_prints() {
    assert_eq!(ask(&NESTED_SCOPE_1), NESTED_SCOPE_1.expected);
}

const NESTED_SCOPE_2: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    try:
        connect()
    except LinkDisconnected as exc:
        class Later:
            raise GatewayError(exc)
        return None
"#),
    ],
    expected: "",
};

#[test]
fn nested_scope_2_prints_what_the_script_prints() {
    assert_eq!(ask(&NESTED_SCOPE_2), NESTED_SCOPE_2.expected);
}

const IMPORT_SHADOWING: Case = Case {
    prompt: "where does the link disconnected error come from",
    files: &[
        ("engine.py", r#"from transport.errors import LinkDisconnected

def perform():
    class LinkDisconnected(Exception):
        pass
    try:
        connect()
    except LinkDisconnected as exc:
        raise GatewayError(exc)
"#),
    ],
    expected: r#"### engine.py:6-9 — conversion/rethrow candidate: matching caught type; runtime message origin unresolved
6|     try:
7|         connect()
8|     except LinkDisconnected as exc:
9|         raise GatewayError(exc)
### engine.py:1-1 — dependency boundary candidate: matching import name; binding unresolved; external emission not inspected
1| from transport.errors import LinkDisconnected
### engine.py:9-9 — local raise/rethrow syntax in this handler; message identity unverified
9|         raise GatewayError(exc)
Call context unresolved: method names alone do not establish receiver identity.
"#,
};

#[test]
fn import_shadowing_prints_what_the_script_prints() {
    assert_eq!(ask(&IMPORT_SHADOWING), IMPORT_SHADOWING.expected);
}

const SECOND_LEVEL_CONTEXT: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def emit():
    raise ValueError("The amber river is frozen")

def first():
    emit()

def entry(enabled):
    if enabled:
        first()
"#),
    ],
    expected: r#"### error origin — local raise syntax; message origin candidate at engine.py:2 in emit
### engine.py:1-8 — candidate source occurrence; inspect the evidence category above
1| def emit():
2|     raise ValueError("The amber river is frozen")
3| 
4| def first():
5|     emit()
6| 
7| def entry(enabled):
8|     if enabled:
### engine.py:4-8 — first → emit: lexical call candidate; receiver/binding unresolved; scope unverified
4| def first():
5|     emit()
6| 
7| def entry(enabled):
8|     if enabled:
### engine.py:7-9 — entry → first: lexical call candidate; receiver/binding unresolved; scope unverified; independent context, no composed path
7| def entry(enabled):
8|     if enabled:
9|         first()
"#,
};

#[test]
fn second_level_context_prints_what_the_script_prints() {
    assert_eq!(ask(&SECOND_LEVEL_CONTEXT), SECOND_LEVEL_CONTEXT.expected);
}

const DIRECT_SITES_NOT_HIDDEN: Case = Case {
    prompt: r#"where is "The amber river is frozen" raised"#,
    files: &[
        ("engine.py", r#"def emit():
    raise ValueError("The amber river is frozen")

def first():
    emit()

def parallel(obj, ready):
    obj.first()
    if not ready:
        emit()
"#),
    ],
    expected: r#"### error origin — local raise syntax; message origin candidate at engine.py:2 in emit
### engine.py:1-8 — candidate source occurrence; inspect the evidence category above
1| def emit():
2|     raise ValueError("The amber river is frozen")
3| 
4| def first():
5|     emit()
6| 
7| def parallel(obj, ready):
8|     obj.first()
### engine.py:4-8 — first → emit: lexical call candidate; receiver/binding unresolved; scope unverified
4| def first():
5|     emit()
6| 
7| def parallel(obj, ready):
8|     obj.first()
### engine.py:7-10 — parallel → emit: lexical call candidate; receiver/binding unresolved; scope unverified
7| def parallel(obj, ready):
8|     obj.first()
9|     if not ready:
10|         emit()
### engine.py:7-10 — parallel → first: lexical call candidate; receiver/binding unresolved; scope unverified; independent context, no composed path
7| def parallel(obj, ready):
8|     obj.first()
9|     if not ready:
10|         emit()
"#,
};

#[test]
fn direct_sites_not_hidden_prints_what_the_script_prints() {
    assert_eq!(ask(&DIRECT_SITES_NOT_HIDDEN), DIRECT_SITES_NOT_HIDDEN.expected);
}

const JS_TRACES_DEFAULT_HANDLER: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"@demo/response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('@demo/response-writer');
function serve(req, res, callback) {
  const done = callback || renderEnd(req, res);
  router.handle(req, res, done);
}
"#),
    ],
    expected: r#"### default handler callback candidate — optional callback falls back to an imported dependency; the handler receives request/response and is passed as the final argument to router.handle; callee behavior and external response text are not inspected
### lib/app.js:1-1 — imported dependency binding
1| const renderEnd = require('@demo/response-writer');
### lib/app.js:2-3 — optional callback fallback
2| function serve(req, res, callback) {
3|   const done = callback || renderEnd(req, res);
### lib/app.js:4-4 — passes the selected handler to router.handle; call behavior unverified
4|   router.handle(req, res, done);
### package.json:1-1 — declared non-development dependency: @demo/response-writer
1| {"dependencies":{"@demo/response-writer":"^1.0.0"}}
"#,
};

#[test]
fn js_traces_default_handler_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_TRACES_DEFAULT_HANDLER), JS_TRACES_DEFAULT_HANDLER.expected);
}

const JS_EXAMPLE_WITHOUT_FALLBACK: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"function serve(req, res) { router.handle(req, res); }
"#),
        ("examples/not-found.js", r#"res.status(404).send('Cannot GET ' + req.url);
"#),
    ],
    expected: "",
};

#[test]
fn js_example_without_fallback_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_EXAMPLE_WITHOUT_FALLBACK), JS_EXAMPLE_WITHOUT_FALLBACK.expected);
}

const JS_BOUNDARY_HIDES_EXAMPLE: Case = Case {
    prompt: "where is the default 404 page that says Cannot GET produced",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) { const done = callback || renderEnd(req,res);
  router.handle(req,res,done); }
"#),
        ("examples/not-found.js", r#"res.status(404).send('Cannot GET ' + req.url);
"#),
    ],
    expected: r#"### default handler callback candidate — optional callback falls back to an imported dependency; the handler receives request/response and is passed as the final argument to router.handle; callee behavior and external response text are not inspected
### lib/app.js:1-1 — imported dependency binding
1| const renderEnd = require('response-writer');
### lib/app.js:1-2 — optional callback fallback
1| const renderEnd = require('response-writer');
2| function serve(req, res, callback) { const done = callback || renderEnd(req,res);
### lib/app.js:3-3 — passes the selected handler to router.handle; call behavior unverified
3|   router.handle(req,res,done); }
### package.json:1-1 — declared non-development dependency: response-writer
1| {"dependencies":{"response-writer":"^1.0.0"}}
"#,
};

#[test]
fn js_boundary_hides_example_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_BOUNDARY_HIDES_EXAMPLE), JS_BOUNDARY_HIDES_EXAMPLE.expected);
}

const JS_UNDECLARED_IMPORT: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"some-other-package":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('unlisted-renderer');
function serve(req, res, callback) { const done = callback || renderEnd(req, res);
  router.handle(req, res, done); }
"#),
    ],
    expected: "",
};

#[test]
fn js_undeclared_import_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_UNDECLARED_IMPORT), JS_UNDECLARED_IMPORT.expected);
}

const JS_SHADOWED_BINDING: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback, renderEnd) {
  const done = callback || renderEnd(req, res);
  router.handle(req, res, done);
}
"#),
    ],
    expected: "",
};

#[test]
fn js_shadowed_binding_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_SHADOWED_BINDING), JS_SHADOWED_BINDING.expected);
}

const JS_FALLBACK_NOT_COMPLETED_0: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function configure(options, callback) { const done = callback || renderEnd(options); builder.handle(options, done); }
"#),
    ],
    expected: "",
};

#[test]
fn js_fallback_not_completed_0_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_FALLBACK_NOT_COMPLETED_0), JS_FALLBACK_NOT_COMPLETED_0.expected);
}

const JS_FALLBACK_NOT_COMPLETED_1: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function choose(req, res, callback) { const done = callback || renderEnd(req, res); }
function route(req, res) { router.handle(req, res, done); }
"#),
    ],
    expected: "",
};

#[test]
fn js_fallback_not_completed_1_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_FALLBACK_NOT_COMPLETED_1), JS_FALLBACK_NOT_COMPLETED_1.expected);
}

const JS_FALLBACK_NOT_COMPLETED_2: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) { const done = renderEnd(req, res); router.handle(req, res, done); }
"#),
    ],
    expected: "",
};

#[test]
fn js_fallback_not_completed_2_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_FALLBACK_NOT_COMPLETED_2), JS_FALLBACK_NOT_COMPLETED_2.expected);
}

const JS_TELEMETRY_0: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) {
  const done = callback || renderEnd(req, res);
  metrics.record(req, res, done);
}
"#),
    ],
    expected: "",
};

#[test]
fn js_telemetry_0_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_TELEMETRY_0), JS_TELEMETRY_0.expected);
}

const JS_TELEMETRY_1: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) {
  const done = callback || renderEnd(req, res);
  router_metrics.finish(req, res, done);
}
"#),
    ],
    expected: "",
};

#[test]
fn js_telemetry_1_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_TELEMETRY_1), JS_TELEMETRY_1.expected);
}

const JS_GENERIC_DISPATCHER: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) {
  const done = callback || renderEnd(req, res);
  request_dispatcher.dispatch(req, res, done);
}
"#),
    ],
    expected: r#"### default handler callback candidate — optional callback falls back to an imported dependency; the handler receives request/response and is passed as the final argument to request_dispatcher.dispatch; callee behavior and external response text are not inspected
### lib/app.js:1-1 — imported dependency binding
1| const renderEnd = require('response-writer');
### lib/app.js:2-3 — optional callback fallback
2| function serve(req, res, callback) {
3|   const done = callback || renderEnd(req, res);
### lib/app.js:4-4 — passes the selected handler to request_dispatcher.dispatch; call behavior unverified
4|   request_dispatcher.dispatch(req, res, done);
### package.json:1-1 — declared non-development dependency: response-writer
1| {"dependencies":{"response-writer":"^1.0.0"}}
"#,
};

#[test]
fn js_generic_dispatcher_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_GENERIC_DISPATCHER), JS_GENERIC_DISPATCHER.expected);
}

const JS_COMMENTS_AND_STRINGS: Case = Case {
    prompt: "Where is the default 404 page produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"// const renderEnd = require('response-writer');
const HELP = `function serve(req, res, callback) { const done = callback || renderEnd(); router.handle(req,res,done); }`;
"#),
    ],
    expected: "",
};

#[test]
fn js_comments_and_strings_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_COMMENTS_AND_STRINGS), JS_COMMENTS_AND_STRINGS.expected);
}

const JS_UNRELATED_PROMPT: Case = Case {
    prompt: "Where is the timeout response produced?",
    files: &[
        ("package.json", r#"{"dependencies":{"response-writer":"^1.0.0"}}
"#),
        ("lib/app.js", r#"const renderEnd = require('response-writer');
function serve(req, res, callback) { const done = callback || renderEnd(req,res);
  router.handle(req,res,done); }
"#),
    ],
    expected: "",
};

#[test]
fn js_unrelated_prompt_prints_what_the_script_prints() {
    assert_eq!(ask(&JS_UNRELATED_PROMPT), JS_UNRELATED_PROMPT.expected);
}

const REQUESTS_WRAPPER: Case = Case {
    prompt: "where does the text of the max retries exceeded connection error come from",
    files: &[
        ("src/requests/adapters.py", r#"from urllib3.exceptions import MaxRetryError
def send():
    try:
        go()
    except MaxRetryError as exc:
        raise ConnectionError(exc)
"#),
    ],
    expected: r#"### src/requests/adapters.py:3-6 — conversion/rethrow candidate: matching caught type; runtime message origin unresolved
3|     try:
4|         go()
5|     except MaxRetryError as exc:
6|         raise ConnectionError(exc)
### src/requests/adapters.py:1-1 — dependency boundary candidate: matching import name; binding unresolved; external emission not inspected
1| from urllib3.exceptions import MaxRetryError
### src/requests/adapters.py:6-6 — local raise/rethrow syntax in this handler; message identity unverified
6|         raise ConnectionError(exc)
Call context unresolved: method names alone do not establish receiver identity.
"#,
};

#[test]
fn requests_wrapper_prints_what_the_script_prints() {
    assert_eq!(ask(&REQUESTS_WRAPPER), REQUESTS_WRAPPER.expected);
}

/// Run `check` on a context over `root`.
fn with_ctx<T>(root: &Path, prompt: &str, check: impl FnOnce(&Ctx) -> T) -> T {
    let ctx = Ctx::new(root, prompt, "", Instant::now() + Duration::from_secs(15));
    check(&ctx)
}

const DIAGNOSTIC_PROMPT: &str = "where is the error printed when a search path does not exist produced";

#[test]
fn the_lexical_search_still_finds_the_neighbour_the_emitter_supersedes() {
    let dir = repo(EMITTER_BEATS_HELP.files);
    let found = with_ctx(dir.path(), DIAGNOSTIC_PROMPT, |ctx| {
        origins(&mut Walk::new(ctx), DIAGNOSTIC_PROMPT).unwrap().unwrap()
    });
    assert_eq!((found.path.as_str(), found.line), ("src/cli.rs", 6));
}

#[test]
fn an_untracked_file_and_a_tracked_symlink_are_not_read_for_an_emitter() {
    let dir = repo(&[("src/main.rs", "fn main() {}\n")]);
    let source = EMITTER_BEATS_HELP.files[0].1;
    std::fs::write(dir.path().join("src/untracked.rs"), source).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.rs"), source).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("outside.rs"),
        dir.path().join("src/external.rs"),
    )
    .unwrap();
    git(dir.path(), &["add", "src/external.rs"]);
    with_ctx(dir.path(), DIAGNOSTIC_PROMPT, |ctx| {
        let paths = tracked_source_paths(ctx);
        assert_eq!(paths, ["src/main.rs"]);
        assert!(error_emission(ctx, &mut Walk::new(ctx), DIAGNOSTIC_PROMPT, &paths).is_none());
    });
}

#[test]
fn an_emitter_needs_two_salient_terms_of_the_question() {
    let dir = repo(EMITTER_BEATS_HELP.files);
    with_ctx(dir.path(), "where is the error printed", |ctx| {
        let paths = tracked_source_paths(ctx);
        assert!(error_emission(ctx, &mut Walk::new(ctx), "where is the error printed", &paths).is_none());
        let found = error_emission(ctx, &mut Walk::new(ctx), DIAGNOSTIC_PROMPT, &paths).unwrap();
        assert_eq!((found.0.as_str(), found.1), ("src/cli.rs", 5));
    });
}

#[test]
fn an_unclosed_string_of_escapes_is_read_in_linear_time() {
    let started = Instant::now();
    assert_eq!(string_part(&format!("x = \"{}", "\\a".repeat(40))), "");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        string_part(r#"err("No files \"were\" found", x)"#),
        r#"No files \"were\" found"#
    );
    assert_eq!(string_part("a 'b' `c`"), "b c");
}

#[test]
fn the_step_admits_the_default_not_found_questions_and_not_the_status_code() {
    for prompt in [
        "where is the default 404 page that says Cannot GET produced",
        "Where is the standard not-found response generated?",
        "Where is the 404 response message generated?",
    ] {
        assert!(STEP.applies(None, prompt, false), "{prompt}");
    }
    for prompt in [
        "What does HTTP status 404 mean?",
        "Where does an unrelated parser produce status code 404?",
        "continue",
        "What is the weather?",
    ] {
        assert!(!STEP.applies(None, prompt, false), "{prompt}");
    }
}

#[test]
fn the_words_of_a_question_are_stemmed_once_without_the_function_words() {
    assert_eq!(
        prompt_words("where does the message about no files being found come from, message files"),
        ["message", "no", "file", "being", "found", "come"]
    );
    assert_eq!(stem("queries"), "query");
    assert_eq!(stem("files"), "file");
    assert_eq!(stem("being"), "being");
    assert_eq!(stem("filed"), "fil");
    assert_eq!(stem("his"), "his");
    assert_eq!(stem("errors"), "error");
}

#[test]
fn quoted_text_needs_twelve_characters() {
    assert_eq!(quoted(r#"where is "The amber river" raised"#), ["The amber river"]);
    assert!(quoted(r#"where is "too short" raised"#).is_empty());
    assert_eq!(quoted("is ‘a long enough text’ here"), ["a long enough text"]);
}

#[test]
fn a_word_is_named_between_non_word_characters() {
    assert!(names_word("t(\"alerts.invalidDrop\")", "alerts.invalidDrop"));
    assert!(!names_word("alerts.invalidDropped", "alerts.invalidDrop"));
    assert!(!names_word("xalerts.invalidDrop", "alerts.invalidDrop"));
    assert!(names_word("a_b a", "a"));
    assert!(!names_word("a_b", "a"));
}

#[test]
fn a_short_word_must_be_a_token_and_a_long_one_starts_a_token() {
    let tokens: HashSet<String> = ["not", "found"].map(str::to_owned).into();
    assert!(!carried("no", &tokens));
    assert!(carried("not", &tokens));
    assert!(!carried("fou", &tokens));
    assert!(carried("foun", &tokens));
    assert!(carried("found", &tokens));
    assert!(!carried("foundation", &tokens));
}

#[test]
fn string_delimiters_are_counted_per_language() {
    assert_eq!(open_quotes("a = \"\"\"x\"\"\" + '''", "a.py"), 3);
    assert_eq!(open_quotes("`a` `", "a.ts"), 3);
    assert_eq!(open_quotes("let c = '\"'; \"x", "a.rs"), 1);
}

#[test]
fn a_run_past_the_clock_prints_nothing() {
    let dir = repo(MESSAGE_FIXTURE_FILES);
    let ctx = Ctx::new(dir.path(), "where is the error printed", "", Instant::now());
    assert_eq!(run_ctx(&ctx), "");
}

const MESSAGE_FIXTURE_FILES: &[(&str, &str)] = DESCRIBED_BEATS_MANUAL.files;

fn run_ctx(ctx: &Ctx) -> String {
    (STEP.run)(ctx)
}
