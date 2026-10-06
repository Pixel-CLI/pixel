// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for concept extraction: which human-meaningful labels each
//! source kind must yield (the text an agent pastes, a status code, a route,
//! a config key) and which it must leave out. The resolver's exact-norm and
//! word tiers only ever find what these rules emitted, so a rule that stops
//! firing makes a pasted label unresolvable, and one that fires too widely
//! buries the right file under noise.

use super::{
    ConceptKind, RawConcept, concept_lang_of, concept_words, extract_concepts, is_test_path,
    normalize,
};

/// `(kind, norm)` of every concept, in emission order.
fn kinds_and_norms(concepts: &[RawConcept]) -> Vec<(ConceptKind, String)> {
    concepts.iter().map(|c| (c.kind, c.norm.clone())).collect()
}

fn of_kind(concepts: &[RawConcept], kind: ConceptKind) -> Vec<String> {
    concepts
        .iter()
        .filter(|c| c.kind == kind)
        .map(|c| c.norm.clone())
        .collect()
}

fn find<'a>(concepts: &'a [RawConcept], kind: ConceptKind, norm: &str) -> &'a RawConcept {
    concepts
        .iter()
        .find(|c| c.kind == kind && c.norm == norm)
        .unwrap_or_else(|| panic!("no {kind:?} {norm:?} in {:?}", kinds_and_norms(concepts)))
}

// --- kinds, normalization, words ----------------------------------------------

#[test]
fn concept_kind_should_round_trip_through_its_stored_name() {
    let all = [
        (ConceptKind::UiText, "ui_text"),
        (ConceptKind::AttrText, "attr_text"),
        (ConceptKind::String, "string"),
        (ConceptKind::Component, "component"),
        (ConceptKind::Form, "form"),
        (ConceptKind::Route, "route"),
        (ConceptKind::Status, "status"),
        (ConceptKind::ConfigKey, "config_key"),
        (ConceptKind::EnvRead, "env_read"),
    ];
    for (kind, name) in all {
        assert_eq!(kind.as_str(), name);
        assert_eq!(ConceptKind::parse(name), kind, "{name}");
    }
}

#[test]
fn concept_kind_parse_should_read_an_unknown_name_as_a_config_key() {
    assert_eq!(ConceptKind::parse("legacy_kind"), ConceptKind::ConfigKey);
    assert_eq!(ConceptKind::parse(""), ConceptKind::ConfigKey);
}

#[test]
fn normalize_should_lowercase_collapse_whitespace_and_trim_edge_punctuation() {
    assert_eq!(
        normalize("  Save   your\n\tChanges!! "),
        "save your changes"
    );
    assert_eq!(normalize("\"Error: disk full.\""), "error: disk full");
    // Inner punctuation is part of the label.
    assert_eq!(normalize("e-mail address"), "e-mail address");
    assert_eq!(normalize("?!..."), "");
}

#[test]
fn normalize_should_compose_unicode_so_both_spellings_of_a_label_match() {
    // "é" typed as one code point and as `e` + combining acute accent.
    let composed = normalize("Caf\u{e9}");
    let decomposed = normalize("Cafe\u{301}");
    assert_eq!(composed, decomposed);
    assert_eq!(composed, "caf\u{e9}");
}

#[test]
fn concept_words_should_split_on_non_alphanumerics_and_keep_distinct_words_of_two_chars() {
    assert_eq!(
        concept_words("save/your changes, save a file 2x"),
        vec!["save", "your", "changes", "file", "2x"]
    );
    assert!(concept_words("a b c").is_empty());
}

#[test]
fn is_test_path_should_match_test_dirs_and_test_file_names() {
    for path in [
        "src/form.spec.ts",
        "src/form.test.tsx",
        "pkg/store_test.go",
        "tests/cli.rs",
        "crates/a/tests/all.rs",
        "lib/test/helper.rb",
        "src/__tests__/cart.ts",
    ] {
        assert!(is_test_path(path), "{path} is a test file");
    }
}

#[test]
fn is_test_path_should_not_match_words_that_only_contain_test() {
    for path in [
        "src/latest.ts",
        "src/contest/form.tsx",
        "src/attestation.rs",
        "testing/util.rs",
        "src/store_tests.rs",
        "Makefile",
    ] {
        assert!(!is_test_path(path), "{path} is production code");
    }
}

#[test]
fn concept_lang_of_should_add_markup_and_config_files_to_the_graph_languages() {
    let cases = [
        ("a.ts", "ts"),
        ("a.mts", "ts"),
        ("a.cts", "ts"),
        ("a.tsx", "tsx"),
        ("a.jsx", "js"),
        ("a.cjs", "js"),
        ("a.rs", "rust"),
        ("a.go", "go"),
        ("A.java", "java"),
        ("a.py", "python"),
        ("a.rake", "ruby"),
        ("App.svelte", "svelte"),
        ("App.vue", "vue"),
        ("index.html", "html"),
        ("package.json", "json"),
        ("ci.yml", "yaml"),
        ("ci.yaml", "yaml"),
        ("site.css", "css"),
    ];
    for (path, lang) in cases {
        assert_eq!(concept_lang_of(path), Some(lang), "{path}");
    }
    for path in ["a.md", "a.toml", "Dockerfile", "a.php"] {
        assert_eq!(concept_lang_of(path), None, "{path}");
    }
}

#[test]
fn languages_without_concept_rules_should_yield_nothing() {
    let src = b"def handler\n  raise \"payment declined by the bank\"\nend\n";
    assert!(extract_concepts("app/models/payment.rb", src).is_empty());
    assert!(
        extract_concepts(
            "svc/main.go",
            b"package main\nvar s = \"three word text here\"\n"
        )
        .is_empty()
    );
}

// --- TypeScript / JavaScript --------------------------------------------------

const TS_SRC: &str = r#"
export async function save(res, app) {
  toast("Saved");
  toast.error("Upload failed");
  alert("Hi");
  console.error("Bad request body");
  console.log("this is ignored by error rules");
  res.status(503);
  res.status(700);
  app.get("/users");
  app.delete("/users/:id");
  app.listen(3000);
  fetch("/api/orders");
  fetch("https://example.com/x");
  abort(404);
  const schema = z.object({ name: z.string() });
  const f1 = useFormik({});
  const f2 = createForm();
  return { status: 201, code: 503, limit: 250, other: { status: 42 } };
}
function fail() {
  throw new Error("Boom");
}
function failTyped() {
  throw new TypeError("Nope");
}
const greeting = `Welcome back to the ${place} dashboard`;
"#;

fn ts_concepts() -> Vec<RawConcept> {
    extract_concepts("src/save.ts", TS_SRC.as_bytes())
}

#[test]
fn error_ish_calls_should_index_every_string_argument_whatever_its_length() {
    let c = ts_concepts();
    let strings = of_kind(&c, ConceptKind::String);
    for label in ["saved", "upload failed", "hi", "bad request body", "boom"] {
        assert!(
            strings.contains(&label.to_string()),
            "{label} in {strings:?}"
        );
    }
}

#[test]
fn only_error_constructors_named_error_should_lift_short_strings() {
    let strings = of_kind(&ts_concepts(), ConceptKind::String);
    // `TypeError("Nope")` is one word: below the plain-string floor, and not
    // an error-ish call by name.
    assert!(!strings.contains(&"nope".to_string()), "{strings:?}");
}

#[test]
fn plain_strings_should_need_three_words_and_twelve_chars() {
    let strings = of_kind(&ts_concepts(), ConceptKind::String);
    assert!(
        strings.contains(&"this is ignored by error rules".to_string()),
        "a long ordinary string is still a string concept: {strings:?}"
    );
    assert!(!strings.contains(&"/users".to_string()), "{strings:?}");
}

#[test]
fn plain_strings_should_be_dropped_below_either_floor_and_kept_at_both() {
    // Floors: 3 words and 12 characters, both inclusive.
    let src = concat!(
        "const a = \"two longwordsonly\";\n", // 2 words, 17 chars
        "const b = \"a b c\";\n",             // 3 words, 5 chars
        "const c = \"ab cd efghi\";\n",       // 3 words, 11 chars
        "const d = \"ab cd efghij\";\n",      // 3 words, 12 chars
    );
    let strings = of_kind(
        &extract_concepts("src/a.ts", src.as_bytes()),
        ConceptKind::String,
    );
    assert_eq!(strings, vec!["ab cd efghij"]);
}

#[test]
fn template_strings_should_index_each_static_fragment_that_is_long_enough() {
    let strings = of_kind(&ts_concepts(), ConceptKind::String);
    assert!(
        strings.contains(&"welcome back to the".to_string()),
        "{strings:?}"
    );
    // `dashboard` alone is one word: below the plain-string floor.
    assert!(!strings.contains(&"dashboard".to_string()), "{strings:?}");
}

#[test]
fn status_concepts_should_come_from_status_positions_within_100_to_599() {
    let c = ts_concepts();
    let statuses = of_kind(&c, ConceptKind::Status);
    assert_eq!(statuses, vec!["503", "404", "201"]);
    let res_status = find(&c, ConceptKind::Status, "503");
    assert_eq!(res_status.detail, "status 503");
}

#[test]
fn app_calls_should_be_routes_only_for_http_method_names() {
    let routes = of_kind(&ts_concepts(), ConceptKind::Route);
    // The route keeps its path, not just the method (#772).
    assert!(routes.contains(&"get /users".to_string()), "{routes:?}");
    assert!(
        routes.contains(&"delete /users/:id".to_string()),
        "{routes:?}"
    );
    assert!(
        !routes
            .iter()
            .any(|r| r.contains("listen") || r.contains("3000")),
        "{routes:?}"
    );
}

/// A comment before the path is not the path.
#[test]
fn app_route_should_skip_a_comment_before_its_path() {
    let c = extract_concepts("src/server.ts", b"app.get(/* note */ \"/users\", list);\n");
    assert_eq!(of_kind(&c, ConceptKind::Route), vec!["get /users"]);
}

#[test]
fn fetch_routes_should_only_index_api_paths() {
    let routes = of_kind(&ts_concepts(), ConceptKind::Route);
    assert!(
        routes.contains(&"fetch /api/orders".to_string()),
        "{routes:?}"
    );
    assert!(
        !routes.iter().any(|r| r.contains("example.com")),
        "{routes:?}"
    );
}

#[test]
fn form_hooks_and_zod_object_schemas_should_each_be_a_form() {
    let forms = of_kind(&ts_concepts(), ConceptKind::Form);
    assert_eq!(
        forms,
        vec!["form", "form", "form"],
        "z.object, useFormik, createForm"
    );
}

#[test]
fn error_strings_in_a_test_file_should_be_skipped_like_any_string() {
    let c = extract_concepts("src/save.test.ts", TS_SRC.as_bytes());
    assert!(
        of_kind(&c, ConceptKind::String).is_empty(),
        "{:?}",
        kinds_and_norms(&c)
    );
    // Only strings are noise in tests: routes and statuses are still facts.
    assert!(!of_kind(&c, ConceptKind::Status).is_empty());
}

#[test]
fn a_string_whose_norm_exceeds_200_chars_should_be_dropped() {
    let long = "word ".repeat(60);
    let src = format!("const s = \"{long}\";\nconst t = \"a short but valid label\";\n");
    let c = extract_concepts("src/a.ts", src.as_bytes());
    assert_eq!(
        of_kind(&c, ConceptKind::String),
        vec!["a short but valid label"]
    );
}

#[test]
fn jsx_should_emit_text_listed_attributes_forms_and_uppercase_components_only() {
    let src = r#"export const V = () => (
  <Form>
    <TextField placeholder="Your email" className="wide primary input" label={dynamic} />
    <img alt="Company logo" data-testid="hero-image" />
    <div>Hello there</div>
    <form />
  </Form>
);
"#;
    let c = extract_concepts("src/V.tsx", src.as_bytes());
    assert_eq!(of_kind(&c, ConceptKind::Component), vec!["textfield"]);
    assert_eq!(of_kind(&c, ConceptKind::Form), vec!["form", "form"]);
    let attrs: Vec<(String, String)> = c
        .iter()
        .filter(|x| x.kind == ConceptKind::AttrText)
        .map(|x| (x.detail.clone(), x.norm.clone()))
        .collect();
    assert_eq!(
        attrs,
        vec![
            ("placeholder".to_string(), "your email".to_string()),
            ("alt".to_string(), "company logo".to_string()),
            ("data-testid".to_string(), "hero-image".to_string()),
        ],
        "className is not a label attribute, and `label={{dynamic}}` has no static value"
    );
    assert!(of_kind(&c, ConceptKind::UiText).contains(&"hello there".to_string()));
}

#[test]
fn jsx_in_a_js_file_should_go_through_the_javascript_grammar() {
    let src = "export const B = () => <button>Pay now</button>;\n";
    let c = extract_concepts("web/B.jsx", src.as_bytes());
    assert_eq!(of_kind(&c, ConceptKind::UiText), vec!["pay now"]);
}

// --- Rust ---------------------------------------------------------------------

#[test]
fn rust_status_codes_should_come_from_status_code_constructors_and_abort() {
    let src = r#"fn h() -> u16 {
    let a = StatusCode::from_u16(503);
    let b = StatusCode::from_u16(42);
    let c = Other::from_u16(404);
    abort(410);
    abort(7);
    0
}
"#;
    let c = extract_concepts("src/server.rs", src.as_bytes());
    assert_eq!(of_kind(&c, ConceptKind::Status), vec!["503", "410"]);
}

#[test]
fn rust_string_literals_should_follow_the_plain_string_floor() {
    let src = "fn f() {\n    let a = \"connection refused by peer\";\n    let b = \"ok\";\n}\n";
    let c = extract_concepts("src/net.rs", src.as_bytes());
    assert_eq!(
        of_kind(&c, ConceptKind::String),
        vec!["connection refused by peer"]
    );
}

#[test]
fn rust_env_reads_should_name_how_the_variable_is_read() {
    let src = r#"fn f() {
    let a = std::env::var_os("PIXEL_HOME");
    let b = option_env!("BUILD_TAG");
    let c = environment::var("NOT_AN_ENV");
    let d = env::var(name);
    let e = env::var("has space");
}
"#;
    let c = extract_concepts("src/cfg.rs", src.as_bytes());
    let reads: Vec<(String, String)> = c
        .iter()
        .filter(|x| x.kind == ConceptKind::EnvRead)
        .map(|x| (x.raw.clone(), x.detail.clone()))
        .collect();
    assert_eq!(
        reads,
        vec![
            (
                "PIXEL_HOME".to_string(),
                "runtime read (env::var_os)".to_string()
            ),
            (
                "BUILD_TAG".to_string(),
                "build-time read (option_env!)".to_string()
            ),
        ]
    );
}

// --- markup: html, svelte, vue --------------------------------------------------

#[test]
fn html_should_skip_comments_doctype_and_closing_tags() {
    let src = "<!doctype html>\n<!-- a comment that is long -->\n<main>\n  <p>Order summary</p>\n</main>\n";
    let c = extract_concepts("public/index.html", src.as_bytes());
    assert_eq!(
        kinds_and_norms(&c),
        vec![(ConceptKind::UiText, "order summary".to_string())]
    );
}

/// Markup lines are 1-based like every other concept's: HTML used to start
/// counting at 0, and Svelte/Vue markup before a `<script>` was numbered
/// from where the script ended (#770).
#[test]
fn markup_concepts_should_report_their_one_based_file_line() {
    let html = extract_concepts(
        "public/a.html",
        b"<p>Order summary</p>\n<p>Pay now please</p>\n",
    );
    assert_eq!(
        find(&html, ConceptKind::UiText, "order summary").start_line,
        1
    );
    assert_eq!(
        find(&html, ConceptKind::UiText, "pay now please").start_line,
        2
    );

    let vue = "<template>\n  <button>Checkout</button>\n</template>\n<script>\nlet a = 1;\n</script>\n<p>Thanks for ordering</p>\n";
    let c = extract_concepts("src/Cart.vue", vue.as_bytes());
    assert_eq!(find(&c, ConceptKind::UiText, "checkout").start_line, 2);
    assert_eq!(
        find(&c, ConceptKind::UiText, "thanks for ordering").start_line,
        7
    );
}

/// Strings in a file under a `__tests__/` directory are test noise, as in a
/// `tests/` one (#771).
#[test]
fn strings_under_a_tests_dunder_directory_should_not_be_concepts() {
    let src = b"const m = \"payment was declined by the bank\";\n";
    assert!(!extract_concepts("src/cart.ts", src).is_empty());
    assert!(
        of_kind(
            &extract_concepts("src/__tests__/cart.ts", src),
            ConceptKind::String
        )
        .is_empty()
    );
}

#[test]
fn html_markup_should_emit_forms_components_and_listed_attributes() {
    let src = "<form action=\"/x\">\n  <input\n    placeholder=\"Search orders\" title=\"\">\n  <Widget name=\"cart\"/>\n</form>\n";
    let c = extract_concepts("public/search.html", src.as_bytes());
    assert_eq!(find(&c, ConceptKind::Form, "form").detail, "form element");
    let placeholder = find(&c, ConceptKind::AttrText, "search orders");
    assert_eq!(placeholder.detail, "placeholder");
    // An attribute on a tag that spans lines is reported at the tag's line.
    assert_eq!(
        placeholder.start_line,
        find(&c, ConceptKind::Form, "form").start_line + 1
    );
    assert_eq!(
        find(&c, ConceptKind::Component, "widget").detail,
        "component"
    );
    // Newlines inside a tag still advance the line count.
    assert_eq!(find(&c, ConceptKind::Component, "widget").start_line, 4);
    assert_eq!(find(&c, ConceptKind::AttrText, "cart").detail, "name");
    // An empty attribute value labels nothing.
    assert_eq!(of_kind(&c, ConceptKind::AttrText).len(), 2);
}

#[test]
fn vue_script_and_markup_should_both_be_scanned_and_script_lines_point_into_the_file() {
    let src = "<template>\n  <button>Checkout</button>\n</template>\n<script>\nexport default { m() { toast(\"Card declined\"); } }\n</script>\n";
    let c = extract_concepts("src/Cart.vue", src.as_bytes());
    find(&c, ConceptKind::UiText, "checkout");
    assert_eq!(find(&c, ConceptKind::String, "card declined").start_line, 5);
    // The script's code never reaches the markup scanner as text.
    assert!(
        !of_kind(&c, ConceptKind::UiText)
            .iter()
            .any(|t| t.contains("toast")),
        "{:?}",
        kinds_and_norms(&c)
    );
}

// --- config files -----------------------------------------------------------------

#[test]
fn json_should_index_dotted_key_paths_and_string_leaves() {
    let src = r#"{"name": "web", "scripts": {"build": "vite build"}, "port": 3000, "tags": ["a"]}"#;
    let c = extract_concepts("package.json", src.as_bytes());
    let pairs: Vec<(String, String)> = c
        .iter()
        .map(|x| (x.norm.clone(), x.detail.clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("name".into(), "key".into()),
            ("web".into(), "value of name".into()),
            ("port".into(), "key".into()),
            ("scripts".into(), "key".into()),
            ("scripts.build".into(), "key".into()),
            ("vite build".into(), "value of scripts.build".into()),
            ("tags".into(), "key".into()),
        ]
    );
    assert!(c.iter().all(|x| x.kind == ConceptKind::ConfigKey));
}

#[test]
fn json_that_does_not_parse_or_is_a_bare_value_should_yield_no_config_key() {
    assert!(extract_concepts("bad.json", b"{ not json").is_empty());
    assert!(extract_concepts("bare.json", b"\"just a string\"").is_empty());
}

#[test]
fn yaml_should_index_nested_keys_by_indentation_and_their_scalar_values() {
    let src = "# header comment\nserver:\n  - name: replica\n  port: 8080 # inline\n  \"host\": example.org\n  tls: {enabled: true}\n  - item\nclient:\n  retries: 3\n";
    let c = extract_concepts("config/app.yml", src.as_bytes());
    let pairs: Vec<(String, String, u32)> = c
        .iter()
        .map(|x| (x.norm.clone(), x.detail.clone(), x.start_line))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("server".into(), "key".into(), 2),
            ("server.port".into(), "key".into(), 4),
            ("8080".into(), "value of server.port".into(), 4),
            ("server.host".into(), "key".into(), 5),
            ("example.org".into(), "value of server.host".into(), 5),
            ("server.tls".into(), "key".into(), 6),
            ("client".into(), "key".into(), 8),
            ("client.retries".into(), "key".into(), 9),
            ("3".into(), "value of client.retries".into(), 9),
        ]
    );
}

#[test]
fn css_should_index_custom_properties_only() {
    let src = ".btn { color: red; }\n:root {\n  --brand-color: #ff0;\n  --gap:4px;\n}\n";
    let c = extract_concepts("styles/site.css", src.as_bytes());
    let props: Vec<(String, u32)> = c.iter().map(|x| (x.raw.clone(), x.start_line)).collect();
    assert_eq!(
        props,
        vec![("--brand-color".to_string(), 3), ("--gap".to_string(), 4)]
    );
    assert!(c.iter().all(|x| x.detail == "css custom property"));
}

// --- file-path routes -------------------------------------------------------------

#[test]
fn next_app_route_should_yield_one_route_per_exported_method_with_its_url() {
    let src = "export async function GET() {}\nexport const POST = async () => {};\n";
    let c = extract_concepts("web/app/api/orders/route.ts", src.as_bytes());
    let routes: Vec<(String, String)> = c
        .iter()
        .filter(|x| x.kind == ConceptKind::Route)
        .map(|x| (x.norm.clone(), x.detail.clone()))
        .collect();
    assert_eq!(
        routes,
        vec![
            (
                "get /api/orders".to_string(),
                "GET web/app/api/orders/route.ts".to_string()
            ),
            (
                "post /api/orders".to_string(),
                "POST web/app/api/orders/route.ts".to_string()
            ),
        ]
    );
}

#[test]
fn a_route_file_at_the_app_root_should_be_the_root_url() {
    let c = extract_concepts("app/route.js", b"export function DELETE() {}\n");
    let routes: Vec<&str> = c
        .iter()
        .filter(|x| x.kind == ConceptKind::Route)
        .map(|x| x.raw.as_str())
        .collect();
    assert_eq!(routes, vec!["DELETE /"]);
}

#[test]
fn a_route_file_without_method_exports_should_yield_no_route() {
    let c = extract_concepts("app/api/x/route.ts", b"export const config = {};\n");
    assert!(of_kind(&c, ConceptKind::Route).is_empty());
}

#[test]
fn sveltekit_server_files_should_route_under_their_routes_directory() {
    let c = extract_concepts(
        "src/routes/cart/items/+server.js",
        b"export async function PATCH() {}\n",
    );
    assert_eq!(of_kind(&c, ConceptKind::Route), vec!["patch /cart/items"]);
}

#[test]
fn pages_api_files_should_be_one_route_named_by_their_path() {
    let c = extract_concepts("pages/api/login.ts", b"export default function h() {}\n");
    let route = find(&c, ConceptKind::Route, "pages/api/login.ts");
    assert_eq!(route.detail, "api pages/api/login.ts");
}

#[test]
fn a_route_named_file_outside_a_route_tree_should_yield_no_route() {
    for path in ["lib/route.ts", "src/myapp/route.ts", "src/routes/+page.ts"] {
        let c = extract_concepts(path, b"export function GET() {}\n");
        assert!(of_kind(&c, ConceptKind::Route).is_empty(), "{path}");
    }
}
