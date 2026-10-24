// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The task tokenizer: the keywords, identifiers and paths a task text names
//! (`specs/brief.md`, "Signal and gate"). The relevance gate and the signal
//! read the same words, so they have one spelling here.

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// Pure-function words that carry no retrieval signal in a task description.
/// Domain-ish words (error, parse, auth, …) deliberately stay searchable.
const STOPWORDS: &[&str] = &[
    // articles / prepositions / pronouns / auxiliaries
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "its",
    "it",
    "a",
    "an",
    "to",
    "in",
    "of",
    "on",
    "at",
    "by",
    "is",
    "are",
    "be",
    "was",
    "were",
    "or",
    "as",
    "so",
    "we",
    "our",
    "my",
    "your",
    "into",
    "from",
    "when",
    "then",
    "than",
    "them",
    "they",
    "there",
    "all",
    "any",
    "can",
    "will",
    "should",
    "must",
    "not",
    "but",
    "without",
    "how",
    "what",
    "why",
    // task verbs / filler
    "fix",
    "add",
    "implement",
    "support",
    "make",
    "create",
    "update",
    "remove",
    "change",
    "refactor",
    "improve",
    "ensure",
    "allow",
    "use",
    "need",
    "want",
    "please",
    "new",
    "also",
    "file",
    "files",
    "code",
    "feature",
    "bug",
    "task",
    "issue",
];

/// French function words, dropped only from a task detected as French
/// ([`TaskLanguage::French`]): several are English words or identifiers
/// (`comment`, `car`, `plus`, `des`, `est`), and dropping them from an
/// English task lost its subject ("fix comment parsing" searched `parsing`).
/// Post-diacritic-fold forms.
const FRENCH_STOPWORDS: &[&str] = &[
    // French articles / prepositions / pronouns / conjunctions
    "le", "la", "les", "un", "une", "des", "du", "de", "et", "ou", "ne", "pas", "que", "qui",
    "dans", "pour", "sur", "avec", "sans", "ce", "cette", "ces", "son", "sa", "ses", "mon", "ma",
    "mes", "ton", "ta", "tes", "notre", "nos", "votre", "vos", "leur", "leurs", "il", "elle",
    "ils", "elles", "nous", "vous", "je", "tu", "se", "lui", "en", "y", "tout", "tous", "toute",
    "toutes", "rien", "personne", "chaque", "quelque", "tres", "plus", "moins", "aussi", "comme",
    "si", "alors", "donc", "mais", "car", "parce",
    // French auxiliaries / question words / adverbs. Post-diacritic-fold forms.
    "est", "sont", "etre", "ete", "avoir", "ont", "faire", "fait", "peut", "doit", "faut",
    "comment", "pourquoi", "quand", "quel", "quelle", "quels", "quelles", "dont", "ca", "cela",
    "meme", "bien", "encore", "deja", "jamais", "toujours", "ici", "ensuite", "puis", "quoi",
];

/// French words no English task uses: two distinct ones (or two
/// unambiguous French relation keys, see [`FRENCH_RELATIONS`]) make a task
/// French. Post-diacritic-fold forms.
const FRENCH_MARKERS: &[&str] = &[
    "le", "les", "une", "des", "du", "et", "pas", "que", "qui", "dans", "pour", "sur", "avec",
    "cette", "ces", "est", "sont", "nous", "vous", "je", "ils", "elles", "mais", "donc",
    "pourquoi", "quand", "quelle", "quels", "quelles", "dont", "cela", "deja", "toujours",
    "jamais", "peut", "doit", "faut", "etre", "avoir", "ne", "au", "aux",
];

/// Distinct French markers a task needs to be treated as French.
const FRENCH_MARKER_THRESHOLD: usize = 2;

/// The language of a task as far as retrieval cares: which stopwords are
/// dropped and whether French relation keys that are also English words
/// expand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskLanguage {
    #[default]
    English,
    French,
}

/// French when [`FRENCH_MARKER_THRESHOLD`] distinct markers occur among the
/// words of `folded_task` (diacritics already folded).
pub fn detect_language(folded_task: &str) -> TaskLanguage {
    let words: HashSet<String> = folded_task
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .collect();
    let relation_keys = FRENCH_RELATIONS
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| !FRENCH_HOMOGRAPHS.contains(key));
    let markers = FRENCH_MARKERS
        .iter()
        .copied()
        .chain(relation_keys)
        .filter(|marker| words.contains(*marker))
        .count();
    if markers >= FRENCH_MARKER_THRESHOLD {
        TaskLanguage::French
    } else {
        TaskLanguage::English
    }
}

const MAX_KEYWORDS: usize = 12;
const MIN_KEYWORD_LEN: usize = 3;

/// French → English cross-lingual mappings (software-development terms).
const FRENCH_RELATIONS: &[(&str, &[&str])] = &[
    // French → English cross-lingual mappings (software-development terms).
    // Keys must be producible by `tokenize_task`: no accents (folded), no
    // underscores (split_ident_words splits on `_`), single words only.
    // Multi-word compounds live in `normalize_task_compounds` instead.
    ("connexion", &["login", "auth", "connection", "session"]),
    ("utilisateur", &["user", "account", "profile"]),
    ("formulaire", &["form", "input", "submit"]),
    ("bouton", &["button", "click", "action"]),
    ("lien", &["link", "navigation", "route"]),
    ("erreur", &["error", "exception", "fault"]),
    ("corriger", &["fix", "repair", "patch", "resolve"]),
    ("ajouter", &["add", "create", "insert"]),
    ("supprimer", &["remove", "delete", "destroy"]),
    ("modifier", &["edit", "update", "modify", "change"]),
    ("afficher", &["display", "show", "render", "view"]),
    ("chercher", &["search", "find", "query", "lookup"]),
    ("sauvegarder", &["save", "store", "persist"]),
    ("charger", &["load", "fetch", "retrieve"]),
    ("envoyer", &["send", "submit", "dispatch"]),
    ("recevoir", &["receive", "fetch", "get"]),
    ("valider", &["validate", "check", "verify"]),
    ("deconnexion", &["logout", "signout", "disconnect"]),
    ("inscription", &["register", "signup"]),
    ("profil", &["profile", "account", "user"]),
    ("message", &["notification", "alert"]),
    ("fichier", &["file", "document", "attachment"]),
    ("dossier", &["folder", "directory", "path"]),
    ("accueil", &["home", "dashboard", "index"]),
    ("parametre", &["settings", "config", "preference"]),
    ("recherche", &["search", "query", "find"]),
    ("resultat", &["result", "output", "response"]),
    ("commande", &["command", "order", "cmd"]),
    ("panier", &["cart", "basket", "order"]),
    ("produit", &["product", "item", "goods"]),
    ("paiement", &["payment", "checkout", "transaction"]),
    ("facture", &["invoice", "bill", "receipt"]),
    ("client", &["customer", "user"]),
    ("serveur", &["server", "backend", "api"]),
    ("requete", &["query", "request", "sql"]),
    ("reponse", &["response", "reply", "result"]),
    ("evenement", &["event", "trigger", "handler"]),
    ("ecouter", &["listen", "subscribe", "observe"]),
    ("arreter", &["stop", "halt", "cancel"]),
    ("demarrer", &["start", "init", "launch", "boot"]),
    // Additional FR→EN: common dev-domain terms.
    ("authentification", &["auth", "login", "authentication"]),
    ("autorisation", &["permission", "auth", "authorize"]),
    ("donnees", &["data", "store", "payload"]),
    ("fonctionnalite", &["feature", "functionality"]),
    ("probleme", &["problem", "issue", "bug"]),
    ("bogue", &["bug", "issue", "defect"]),
    ("echec", &["failure", "error", "fail"]),
    ("panne", &["failure", "outage", "error"]),
    ("cle", &["key", "token"]),
    ("valeur", &["value", "val"]),
    ("champ", &["field", "column", "property"]),
    ("entree", &["input", "entry"]),
    ("sortie", &["output", "exit"]),
    ("fonction", &["function", "method", "fn"]),
    ("variable", &["variable", "var"]),
    ("chaine", &["string", "str"]),
    ("tableau", &["array", "table", "list"]),
    ("liste", &["list", "array"]),
    ("boucle", &["loop", "iterate"]),
    ("tri", &["sort", "order"]),
    ("filtre", &["filter", "where"]),
    ("journal", &["log", "journal"]),
    ("deboguer", &["debug", "troubleshoot"]),
    ("deploiement", &["deploy", "deployment"]),
    ("securite", &["security", "auth", "permission"]),
    ("reseau", &["network", "net"]),
    ("memoire", &["memory", "cache"]),
    ("compte", &["account", "user", "profile"]),
    ("identifiant", &["identifier", "id", "username"]),
    ("courriel", &["email", "mail"]),
    ("adresse", &["address", "url", "email"]),
    ("retour", &["return", "back"]),
    ("ouvrir", &["open", "read"]),
    ("fermer", &["close", "shutdown"]),
    ("lire", &["read", "load"]),
    ("ecrire", &["write", "save"]),
    ("creer", &["create", "make"]),
    ("executer", &["run", "execute"]),
    ("appeler", &["call", "invoke"]),
    ("installer", &["install", "setup"]),
    ("configurer", &["configure", "setup"]),
    ("tache", &["task", "job"]),
    ("etat", &["state", "status"]),
    ("modele", &["model", "schema"]),
    ("vue", &["view", "render"]),
    ("controleur", &["controller", "handler"]),
    ("dependance", &["dependency", "dep"]),
    ("contenu", &["content", "body"]),
    ("nom", &["name", "label"]),
    ("numero", &["number", "id"]),
    ("prix", &["price", "cost"]),
    ("taille", &["size", "length"]),
    ("couleur", &["color", "style"]),
    ("theme", &["theme", "style"]),
    ("image", &["image", "img", "icon"]),
    ("onglet", &["tab"]),
    ("menu", &["menu", "navigation"]),
    ("fenetre", &["window", "dialog"]),
    ("ecran", &["screen", "display"]),
    ("affichage", &["display", "render"]),
    ("chargement", &["loading", "load"]),
    ("enregistrer", &["save", "store"]),
    ("telecharger", &["download", "fetch"]),
    ("importer", &["import"]),
    ("exporter", &["export"]),
    ("synchroniser", &["sync", "synchronize"]),
    ("deployer", &["deploy"]),
];

/// [`FRENCH_RELATIONS`] keys that are also English words: in an English task
/// `client` is an HTTP client, not a customer, and `modifier` a keyword, not
/// "edit". They expand only in a French task.
const FRENCH_HOMOGRAPHS: &[&str] = &[
    "message",
    "client",
    "dossier",
    "variable",
    "image",
    "menu",
    "theme",
    "journal",
    "vue",
    "importer",
    "exporter",
    "installer",
    "inscription",
    "charger",
    "modifier",
    "lien",
    "tableau",
];

pub const SHORT_TECH_KEYWORDS: &[&str] = &[
    "c", "r", "go", "rs", "ts", "js", "py", "rb", "sh", "ui", "ci", "cd", "db", "os", "io", "ip",
    "ai", "ml",
];

/// Normalize compound technical terms (e.g. "c#", "c++", ".net", "node.js")
/// into clean alphanumeric words before word splitting.
pub fn normalize_task_compounds(task: &str) -> String {
    let mut out = task.to_string();
    let replacements = [
        ("c#", "csharp"),
        ("c-sharp", "csharp"),
        ("c_sharp", "csharp"),
        ("ruby on rails", "ruby"),
        ("c++", "cpp"),
        ("f#", "fsharp"),
        (".net", "dotnet"),
        ("node.js", "nodejs"),
        ("vue.js", "vuejs"),
        ("next.js", "nextjs"),
        ("react.js", "reactjs"),
        // French multi-word compounds (post-diacritic-fold). These must be
        // replaced before tokenization because `split_ident_words` would
        // split them into unrecognizable parts.
        ("mot de passe", "password"),
        ("base de donnees", "database"),
        ("coup de main", "help"),
    ];
    for (from, to) in replacements {
        let mut lower = out.to_ascii_lowercase();
        while let Some(idx) = lower.find(from) {
            let mut result = String::with_capacity(out.len() + to.len());
            result.push_str(&out[..idx]);
            result.push_str(to);
            result.push_str(&out[idx + from.len()..]);
            out = result;
            lower = out.to_ascii_lowercase();
        }
    }
    out
}

/// Static, code-domain thesaurus for semantic keyword expansion. Each keyword
/// is mapped to a small, conservative set of related terms used only to
/// broaden path/filename matching; the original keywords remain canonical for
/// content probes and report labels.
const SEMANTIC_RELATIONS: &[(&str, &[&str])] = &[
    ("login", &["auth", "authenticate", "signin", "session"]),
    ("auth", &["login", "authenticate", "session", "user"]),
    ("authenticate", &["login", "auth", "session", "user"]),
    ("signin", &["login", "auth", "authenticate"]),
    ("user", &["account", "login", "profile"]),
    ("account", &["user", "login", "profile"]),
    ("session", &["cookie", "login", "auth", "token"]),
    ("cookie", &["session", "auth", "login"]),
    ("token", &["jwt", "session", "auth", "credential"]),
    ("jwt", &["token", "auth", "session"]),
    ("credential", &["token", "password", "auth", "login"]),
    ("password", &["credential", "auth", "login"]),
    ("config", &["settings", "configuration", "options"]),
    ("settings", &["config", "configuration", "options"]),
    ("configuration", &["config", "settings", "options"]),
    ("database", &["sql", "query", "schema"]),
    ("sql", &["database", "query"]),
    ("query", &["sql", "database", "request"]),
    ("api", &["endpoint", "route", "request", "response"]),
    ("endpoint", &["api", "route", "request"]),
    ("request", &["response", "api", "route", "endpoint"]),
    ("response", &["request", "api", "route", "endpoint"]),
    ("route", &["endpoint", "api", "request"]),
    ("error", &["exception", "failure", "panic"]),
    ("exception", &["error", "failure"]),
    ("failure", &["error", "exception"]),
    ("panic", &["error", "exception"]),
    ("cache", &["store", "storage"]),
    ("store", &["cache", "storage"]),
    ("storage", &["cache", "store"]),
    ("queue", &["job", "worker", "background"]),
    ("job", &["queue", "worker", "task"]),
    ("worker", &["queue", "job", "background"]),
    ("background", &["queue", "job", "worker"]),
    ("email", &["mail"]),
    ("mail", &["email"]),
    // Languages, compilers, parser & AST
    (
        "csharp",
        &["language", "extract", "grammar", "c_sharp", "cs"],
    ),
    ("python", &["language", "extract", "grammar", "py"]),
    ("ruby", &["language", "extract", "grammar", "rb", "rails"]),
    ("rust", &["language", "extract", "grammar", "rs"]),
    ("javascript", &["language", "extract", "grammar", "js"]),
    ("typescript", &["language", "extract", "grammar", "ts"]),
    ("golang", &["language", "extract", "grammar", "go"]),
    ("java", &["language", "extract", "grammar"]),
    ("cpp", &["language", "extract", "grammar", "cplusplus"]),
    ("language", &["extract", "grammar", "parser", "syntax"]),
    ("parser", &["language", "extract", "grammar", "syntax"]),
    ("grammar", &["language", "extract", "parser"]),
];

/// Expand a single keyword of a task in `language` to its semantic relatives.
pub fn semantic_expand(word: &str, language: TaskLanguage) -> Vec<&'static str> {
    let french = language == TaskLanguage::French || !FRENCH_HOMOGRAPHS.contains(&word);
    let french_relations: &[(&str, &[&str])] = if french { FRENCH_RELATIONS } else { &[] };
    SEMANTIC_RELATIONS
        .iter()
        .chain(french_relations)
        .find(|(term, _)| *term == word)
        .map(|(_, related)| related.to_vec())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    /// Backticked/quoted identifiers taken verbatim: `` `login_user` `` →
    /// exact symbol-name probe (case preserved). Unquoted `snake_case` words
    /// are identifiers too and land here as well.
    pub exact_tokens: Vec<String>,
    /// File paths named in the task (`upgrade_cli.rs`, `src/auth.rs`),
    /// matched against the live tree by the path signal.
    pub path_tokens: Vec<String>,
    /// Lowercased keywords, first-occurrence order, deduped, len ≥ 3 (or short tech keyword), ≤ 12.
    pub keywords: Vec<String>,
    /// True when the task contained MORE searchable keywords than
    /// [`MAX_KEYWORDS`]: the dropped words contributed no signal, so any
    /// result built from this query is a lower bound, not exhaustive.
    pub keywords_truncated: bool,
    /// Detected language: selects the stopwords and the expansions.
    pub language: TaskLanguage,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Extensions that mark a dotted token as a file path rather than an
/// identifier or a sentence-ending abbreviation. Lowercase; compared
/// case-insensitively.
const PATH_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "rb", "go", "java", "kt", "kts", "swift",
    "c", "h", "cc", "cpp", "cxx", "hpp", "cs", "php", "sh", "bash", "zsh", "fish", "sql", "toml",
    "yaml", "yml", "json", "md", "vue", "svelte", "ex", "exs", "erl", "hs", "ml", "scala", "clj",
    "lua", "pl", "dart", "zig", "nim",
];

/// True when `ext` names a source/config file extension.
fn is_path_extension(ext: &str) -> bool {
    let lower = ext.to_ascii_lowercase();
    PATH_EXTENSIONS.contains(&lower.as_str())
}

/// The basename stem of a path token: the last `/`-component with its
/// extension removed (`crates/a/b_login.rs` → `b_login`).
fn path_stem(token: &str) -> &str {
    let base = token.rsplit('/').next().unwrap_or(token);
    base.rsplit_once('.').map_or(base, |(stem, _)| stem)
}

/// Validate a candidate run as a file path: it has a `.` whose suffix is a
/// known source extension and a nonempty stem. Returns the token as written.
fn path_token(run: &str) -> Option<String> {
    let trimmed = run.trim_matches(|c| matches!(c, '.' | '/' | '-'));
    let stem = path_stem(trimmed);
    let ext = trimmed.rsplit_once('.').map(|(_, ext)| ext)?;
    if stem.is_empty() || !is_path_extension(ext) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Split file paths out of the task text: every run of `[A-Za-z0-9_./-]`
/// whose last `.`-suffix is a known source extension is recorded as a path
/// token and replaced in the returned text by its basename stem, so the
/// generic word pass contributes `upgrade`/`cli` but never the extension
/// `rs` (a word in the filename of every Rust file, which would hand the
/// whole tree a filename hit).
fn extract_path_tokens(text: &str) -> (String, Vec<String>) {
    fn is_run_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')
    }
    // Scan boundaries first, rebuild in a second `for`: a hand-advanced
    // index can spin forever on a mutated bound, and a hanging mutant costs
    // the whole mutation timeout instead of failing an assertion.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, c) in text.char_indices() {
        if is_run_char(c) {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(run_start) = start.take() {
            runs.push((run_start, index));
        }
    }
    if let Some(run_start) = start {
        runs.push((run_start, text.len()));
    }
    let mut out = String::with_capacity(text.len());
    let mut paths: Vec<String> = Vec::new();
    let mut copied = 0usize;
    for (run_start, run_end) in runs {
        out.push_str(&text[copied..run_start]);
        copied = run_end;
        match path_token(&text[run_start..run_end]) {
            Some(token) => {
                if !paths.contains(&token) {
                    paths.push(token.clone());
                }
                out.push_str(path_stem(&token));
            }
            None => out.push_str(&text[run_start..run_end]),
        }
    }
    out.push_str(&text[copied..]);
    (out, paths)
}

/// Fold diacritics to their ASCII base so accented characters don't act as
/// token delimiters. `déconnexion` → `deconnexion`, `paramètre` → `parametre`.
/// Without this, `tokenize_task` would split on `é`/`è`/`à`/`ç` and produce
/// garbage keywords (`déconnexion` → `d` + `connexion` → expands to *login*
/// instead of *logout* — a semantic inversion).
fn fold_diacritics(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect()
}

/// Tokenize a task description. Errors when nothing searchable survives.
pub fn tokenize_task(task: &str) -> Result<TaskQuery, String> {
    // Fold diacritics BEFORE compound normalization so accented multi-word
    // terms ("base de données" → "base de donnees") can match their
    // replacements in `normalize_task_compounds`.
    let normalized = normalize_task_compounds(&fold_diacritics(task));
    // Paths are lifted out before the generic word pass: their extension is
    // not a keyword (`rs` would match every Rust file's name) and their
    // basename stem is the searchable part.
    let (text, path_tokens) = extract_path_tokens(&normalized);
    let language = detect_language(&text);
    let french_stopwords: &[&str] = match language {
        TaskLanguage::French => FRENCH_STOPWORDS,
        TaskLanguage::English => &[],
    };
    let stop: HashSet<&str> = STOPWORDS.iter().chain(french_stopwords).copied().collect();
    let mut exact_tokens: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut seen_kw: HashSet<String> = HashSet::default();
    let mut keywords_truncated = false;

    let push_words = |text: &str,
                      keywords: &mut Vec<String>,
                      seen: &mut HashSet<String>,
                      truncated: &mut bool,
                      exact_tokens: &mut Vec<String>| {
        for chunk in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            // An underscore-joined word is a code identifier, not English
            // (`cap_message`, `apply_change`): probe it as an exact symbol
            // name even without backticks. CamelCase alone is not enough —
            // ordinary capitalized words would exact-match symbols by
            // accident.
            if is_ident(chunk) && chunk.contains('_') && !exact_tokens.iter().any(|t| t == chunk) {
                exact_tokens.push(chunk.to_string());
            }
            for w in split_ident_words(chunk) {
                let is_valid_len =
                    w.len() >= MIN_KEYWORD_LEN || SHORT_TECH_KEYWORDS.contains(&w.as_str());
                if is_valid_len && !stop.contains(w.as_str()) {
                    if keywords.len() >= MAX_KEYWORDS {
                        // A distinct searchable word was dropped by the cap —
                        // record it so callers can surface the truncation.
                        if !seen.contains(&w) {
                            *truncated = true;
                        }
                        continue;
                    }
                    if seen.insert(w.clone()) {
                        keywords.push(w);
                    }
                }
            }
        }
    };

    // Pass 1: quoted spans become exact tokens AND feed the keyword pass.
    let mut rest = String::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => {
                if is_ident(&cur) && !exact_tokens.contains(&cur) {
                    exact_tokens.push(cur.clone());
                }
                rest.push(' ');
                rest.push_str(&cur);
                cur.clear();
                quote = None;
            }
            Some(_) => cur.push(c),
            None if c == '`' || c == '"' || c == '\'' => quote = Some(c),
            None => rest.push(c),
        }
    }
    if !cur.is_empty() {
        rest.push(' ');
        rest.push_str(&cur);
    }
    push_words(
        &rest,
        &mut keywords,
        &mut seen_kw,
        &mut keywords_truncated,
        &mut exact_tokens,
    );

    if exact_tokens.is_empty() && keywords.is_empty() {
        return Err("task description yields no searchable keywords".to_string());
    }
    Ok(TaskQuery {
        exact_tokens,
        path_tokens,
        keywords,
        keywords_truncated,
        language,
    })
}

/// Word-frequency helper: split camelCase + snake_case identifiers into
/// lowercase words.
pub fn split_ident_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for chunk in name.split(['_', '-', '.', ':', '#']) {
        if chunk.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = chunk.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = c.is_uppercase()
                && i > 0
                && (chars[i - 1].is_lowercase()
                    || (i + 1 < chars.len() && chars[i + 1].is_lowercase()));
            if boundary && !cur.is_empty() {
                words.push(cur.to_lowercase());
                cur = String::new();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            words.push(cur.to_lowercase());
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_idents_and_drops_stopwords() {
        let q = tokenize_task("fix loginUser session handling in AuthService").unwrap();
        assert_eq!(
            q.keywords,
            vec!["login", "user", "session", "handling", "auth", "service"]
        );
        assert!(q.exact_tokens.is_empty());
    }
    #[test]
    fn tokenize_extracts_backticked_identifiers() {
        let q = tokenize_task("`login_user` panics on empty token").unwrap();
        assert_eq!(q.exact_tokens, vec!["login_user"]);
        assert_eq!(
            q.keywords,
            vec!["login", "user", "panics", "empty", "token"]
        );
    }
    /// French function words that are English words stay searchable in an
    /// English task: "fix comment parsing" is about comments.
    #[test]
    fn english_task_keeps_words_that_are_french_stopwords() {
        let q = tokenize_task("fix comment parsing for car plus sans rental").unwrap();
        assert_eq!(q.language, TaskLanguage::English);
        assert_eq!(
            q.keywords,
            vec!["comment", "parsing", "car", "plus", "sans", "rental"]
        );
    }
    #[test]
    fn french_task_drops_french_function_words() {
        let q = tokenize_task("Pourquoi la connexion est très lente quand même ?").unwrap();
        assert_eq!(q.language, TaskLanguage::French);
        assert_eq!(q.keywords, vec!["connexion", "lente"]);

        let q = tokenize_task("comment corriger le bouton de connexion").unwrap();
        assert_eq!(q.language, TaskLanguage::French);
        assert_eq!(q.keywords, vec!["corriger", "bouton", "connexion"]);
    }
    #[test]
    fn two_distinct_french_markers_make_a_task_french() {
        use TaskLanguage::{English, French};
        assert_eq!(detect_language("fix the login"), English);
        assert_eq!(detect_language("le login"), English, "one marker");
        assert_eq!(
            detect_language("le le login"),
            English,
            "one distinct marker"
        );
        assert_eq!(detect_language("le login est lent"), French);
        assert_eq!(
            detect_language("bouton connexion casse"),
            French,
            "unambiguous French relation keys count"
        );
        assert_eq!(
            detect_language("client message queue"),
            English,
            "homograph keys are English words and prove nothing"
        );
        assert_eq!(detect_language("LE login EST lent"), French, "case-folded");
        assert_eq!(detect_language(""), English);
    }
    #[test]
    fn diacritics_fold_before_compounds_and_tokens() {
        assert_eq!(
            fold_diacritics("déconnexion paramètre très façade"),
            "deconnexion parametre tres facade"
        );
        let q = tokenize_task("réparer la déconnexion de la base de données et le mot de passe")
            .unwrap();
        assert_eq!(q.language, TaskLanguage::French);
        assert_eq!(
            q.keywords,
            vec!["reparer", "deconnexion", "database", "password"]
        );
        assert_eq!(
            normalize_task_compounds("C# and c# then C++"),
            "csharp and csharp then cpp"
        );
    }
    #[test]
    fn tokenize_all_stopwords_errors() {
        assert!(tokenize_task("fix the code in a file").is_err());
        assert!(tokenize_task("").is_err());
    }
    /// A task that names a file must not turn the extension into a keyword:
    /// `rs` is a word in the filename of every Rust file, so it would hand
    /// the whole tree a filename hit.
    #[test]
    fn tokenize_lifts_file_paths_out_of_the_keywords() {
        let q = tokenize_task(
            "de-flake upgrade_reports_unresponsive_daemon_without_claiming_completion test in upgrade_cli.rs",
        )
        .unwrap();
        assert_eq!(q.path_tokens, vec!["upgrade_cli.rs"]);
        assert!(
            q.keywords.contains(&"upgrade".to_string()),
            "{:?}",
            q.keywords
        );
        assert!(q.keywords.contains(&"cli".to_string()), "{:?}", q.keywords);
        assert!(!q.keywords.contains(&"rs".to_string()), "{:?}", q.keywords);
        // An underscore-joined blob is an identifier candidate, not a
        // symbol name that exists — harmless, and the words still search.
        assert!(
            q.exact_tokens.contains(
                &"upgrade_reports_unresponsive_daemon_without_claiming_completion".to_string()
            ),
            "{:?}",
            q.exact_tokens
        );
    }
    #[test]
    fn tokenize_full_path_keeps_only_the_basename_stem() {
        let q = tokenize_task("fix the timeout in crates/pixel/tests/cli/upgrade_cli.rs").unwrap();
        assert_eq!(q.path_tokens, vec!["crates/pixel/tests/cli/upgrade_cli.rs"]);
        assert_eq!(q.keywords, vec!["timeout", "upgrade", "cli"]);
        // The basename stem is itself an underscore identifier.
        assert_eq!(q.exact_tokens, vec!["upgrade_cli"]);
    }
    /// An unquoted `snake_case` word is a code identifier: the exact-name
    /// probe must see it, because the task named the function itself.
    #[test]
    fn tokenize_unquoted_snake_identifier_is_exact() {
        let q = tokenize_task("cap_message finds the boundary without a mutable loop").unwrap();
        assert_eq!(q.exact_tokens, vec!["cap_message"]);
        assert!(q.keywords.contains(&"cap".to_string()));
        assert!(q.keywords.contains(&"message".to_string()));
        assert!(
            !q.keywords.contains(&"without".to_string()),
            "{:?}",
            q.keywords
        );
    }
    #[test]
    fn tokenize_records_each_named_path_once() {
        let q = tokenize_task("touch guard.rs, then guard.rs again, and rename.rs").unwrap();
        assert_eq!(q.path_tokens, vec!["guard.rs", "rename.rs"]);
        assert!(
            q.keywords.contains(&"rename".to_string()),
            "{:?}",
            q.keywords
        );
    }
    #[test]
    fn tokenize_leaves_unknown_dotted_words_alone() {
        // `flake.lock` has a dot but no source extension: it is not a path,
        // and both words stay in the keyword text.
        let q = tokenize_task("bump the flake.lock pin to stable").unwrap();
        assert!(q.path_tokens.is_empty(), "{:?}", q.path_tokens);
        assert!(
            q.keywords.contains(&"flake".to_string()),
            "{:?}",
            q.keywords
        );
        assert!(q.keywords.contains(&"lock".to_string()), "{:?}", q.keywords);
        // A basename that is nothing but an extension (`x/.rs`) is not a
        // path: the stem would be empty once the dot is removed.
        let q = tokenize_task("remove the stale x/.rs entry").unwrap();
        assert!(q.path_tokens.is_empty(), "{:?}", q.path_tokens);
    }
    #[test]
    fn tokenize_technical_compounds_and_short_keywords() {
        let q_csharp = tokenize_task("add c# support").unwrap();
        assert_eq!(q_csharp.keywords, vec!["csharp"]);

        let q_cpp = tokenize_task("add c++ support").unwrap();
        assert_eq!(q_cpp.keywords, vec!["cpp"]);

        let q_go = tokenize_task("add go support").unwrap();
        assert_eq!(q_go.keywords, vec!["go"]);

        let q_dotnet = tokenize_task("support for .NET and Node.js").unwrap();
        assert_eq!(q_dotnet.keywords, vec!["dotnet", "nodejs"]);

        let q_ruby = tokenize_task("add ruby on rails support").unwrap();
        assert_eq!(q_ruby.keywords, vec!["ruby"]);
    }

    /// `client` and `message` are English words: an English task keeps its
    /// meaning, a French one gets the translation.
    #[test]
    fn homograph_keys_expand_only_in_a_french_task() {
        assert!(semantic_expand("client", TaskLanguage::English).is_empty());
        assert!(semantic_expand("message", TaskLanguage::English).is_empty());
        assert_eq!(
            semantic_expand("client", TaskLanguage::French),
            ["customer", "user"]
        );
        // An unambiguous French key expands whatever the detected language.
        assert_eq!(
            semantic_expand("connexion", TaskLanguage::English),
            ["login", "auth", "connection", "session"]
        );
        // General relations do not depend on the language.
        assert_eq!(
            semantic_expand("cookie", TaskLanguage::French),
            ["session", "auth", "login"]
        );
        assert!(semantic_expand("unrelated", TaskLanguage::French).is_empty());
    }
}
