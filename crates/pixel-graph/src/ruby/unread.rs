// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What Ruby text the grammar could not read may define, from its words
//! alone: the names a resolution must not prove anything against.
//!
//! tree-sitter-ruby 0.23 cannot parse some valid files (an anonymous block
//! argument after a line continuation, `link_to url, &`): the region becomes
//! an `ERROR` node whose definitions the walker cannot place, so they are
//! missing from the graph, and a lookup that would have stopped at one of
//! them goes on to another definition (D12). [`scan`] reads such a region as
//! plain text and lists every name it may define. The list errs on the side
//! of more names, never fewer: a word in a string or after a `#` counts,
//! and only a line that is a comment from its start is skipped. The same
//! scan serves a file that is not parsed at all (past the size cap).
//!
//! Resolution then proves nothing those names could change: a constant
//! path with an unread segment, a method lookup through an owner the region
//! may reopen when the name is unread there (any name when the region may
//! add an ancestor or a computed method), a top-level function of an
//! unread name, and any site of a class the region may reopen. An owner the
//! region may reopen is one it opens, one its definers and mixins are sent
//! to (`Base.prepend(..)`), and any its file parses; a name it defines
//! where the scan cannot tell on what ([`nest`]: a block run elsewhere,
//! `def obj.m`) is no target anywhere (B9).

/// What an unread region may define. Each list is sorted, without repeats.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    /// Method names: `def m`, `def self.m`, an accessor, an alias, a
    /// `define_method`, a `module_function`, `delegate` or `scope` name.
    pub methods: Vec<String>,
    /// Last segments of the classes and modules it opens or reopens
    /// (`class A::B`, `module M`, `class << X`, `def X.m`).
    pub owners: Vec<String>,
    /// Last segments of the constants it may define: its classes and
    /// modules, and constant assignments.
    pub constants: Vec<String>,
    /// It may add an ancestor (`include`, `extend`, `prepend`) or define a
    /// method whose name is computed, so any name of its owners.
    pub any: bool,
    /// Method names it may define where the scan cannot tell on what (a
    /// block run elsewhere, `def obj.m`, a definer sent to another
    /// object): any class or module may hold them (B9, [`nest`]).
    pub loose: Vec<String>,
    /// It may add an ancestor, or a method of any name, to an object the
    /// scan cannot name.
    pub loose_any: bool,
}

mod nest;

/// Calls whose literal arguments name the methods they define or touch.
const DECLARATIONS: &[&str] = &[
    "attr_reader",
    "attr_writer",
    "attr_accessor",
    "attr",
    "alias_method",
    "define_method",
    "define_singleton_method",
    "delegate",
    "scope",
    "module_function",
    "remove_method",
    "undef_method",
];

impl Unread {
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
            && self.owners.is_empty()
            && self.constants.is_empty()
            && self.loose.is_empty()
            && !self.any
            && !self.loose_any
    }

    /// Adds what `other` may define.
    pub fn merge(&mut self, other: Unread) {
        self.methods.extend(other.methods);
        self.owners.extend(other.owners);
        self.constants.extend(other.constants);
        self.loose.extend(other.loose);
        self.any |= other.any;
        self.loose_any |= other.loose_any;
        self.settle();
    }

    fn settle(&mut self) {
        let lists = [
            &mut self.methods,
            &mut self.owners,
            &mut self.constants,
            &mut self.loose,
        ];
        for list in lists {
            list.sort();
            list.dedup();
        }
    }

    /// The fact's record: `unread`, `any` or empty, then `m=`, `o=`, `c=`,
    /// `l=` fields, and `L` when [`Self::loose_any`].
    pub(super) fn record(&self) -> Vec<String> {
        let mut record = vec![
            "unread".to_owned(),
            if self.any { "any" } else { "" }.to_owned(),
        ];
        if self.loose_any {
            record.push("L".to_owned());
        }
        let fields = [
            ("m=", &self.methods),
            ("o=", &self.owners),
            ("c=", &self.constants),
            ("l=", &self.loose),
        ];
        for (prefix, names) in fields {
            record.extend(names.iter().map(|name| format!("{prefix}{name}")));
        }
        record
    }

    /// The inverse of [`Self::record`], given the fields after the tag.
    pub(super) fn from_record(fields: &[String]) -> Unread {
        let mut unread = Unread {
            any: fields.first().is_some_and(|f| f == "any"),
            ..Unread::default()
        };
        for field in fields.iter().skip(1) {
            if let Some(name) = field.strip_prefix("m=") {
                unread.methods.push(name.to_owned());
            } else if let Some(name) = field.strip_prefix("o=") {
                unread.owners.push(name.to_owned());
            } else if let Some(name) = field.strip_prefix("c=") {
                unread.constants.push(name.to_owned());
            } else if let Some(name) = field.strip_prefix("l=") {
                unread.loose.push(name.to_owned());
            } else if field == "L" {
                unread.loose_any = true;
            }
        }
        unread.settle();
        unread
    }
}

/// Every name `text` may define, read word by word: no grammar, so no
/// region is too broken to scan.
pub fn scan(text: &str) -> Unread {
    let mut unread = Unread::default();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for statement in line.split(';') {
            read_statement(&tokens(statement), &mut unread);
        }
    }
    nest::read(text, &mut unread);
    unread.settle();
    unread
}

/// A word of Ruby text: an identifier or constant (with a trailing `?`,
/// `!` or a setter's `=`), a `:symbol`, a quoted name, or one character of
/// punctuation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Symbol(String),
    Quoted(String),
    Punct(char),
}

/// A word may hold [`INTERPOLATED`], which stands for an interpolation in
/// code a string evaluates: `def _run_#{name}_callbacks` defines a name of
/// that pattern.
fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == INTERPOLATED
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == INTERPOLATED
}

/// What stands for an interpolation in the code a string evaluates, in a
/// name it may define: any text.
pub const INTERPOLATED: char = '\u{1}';

/// Whether the method `name` fits `pattern`, a name that may hold
/// [`INTERPOLATED`] (any text, maybe none). Each piece between two of them
/// is taken where it first fits, which decides as well as any other place
/// would, in one pass and without recursion.
pub fn fits(pattern: &str, name: &str) -> bool {
    let mut pieces = pattern.split(INTERPOLATED);
    let Some(mut rest) = name.strip_prefix(pieces.next().unwrap_or_default()) else {
        return false;
    };
    let pieces: Vec<&str> = pieces.collect();
    let Some((last, middle)) = pieces.split_last() else {
        return rest.is_empty();
    };
    for piece in middle {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// Whether `code`, which a string evaluates, may run what an interpolation
/// holds, not only name with it: one stands for a whole statement.
pub fn runs_interpolated(code: &str) -> bool {
    code.lines()
        .flat_map(|line| line.split(';'))
        .any(|statement| statement.trim_start().starts_with(INTERPOLATED))
}

fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let word_at = |mut j: usize| -> (String, usize) {
        let start = j;
        while j < chars.len() && is_word_char(chars[j]) {
            j += 1;
        }
        if j < chars.len() && matches!(chars[j], '?' | '!') {
            j += 1;
        }
        (chars[start..j].iter().collect(), j)
    };
    while i < chars.len() {
        let c = chars[i];
        if is_word_start(c) {
            let (word, next) = word_at(i);
            out.push(Token::Word(word));
            i = next;
        } else if c == ':'
            && chars.get(i + 1).is_some_and(|n| is_word_start(*n))
            && (i == 0 || chars[i - 1] != ':')
        {
            let (word, next) = word_at(i + 1);
            out.push(Token::Symbol(word));
            i = next;
        } else if matches!(c, '"' | '\'') {
            // A quoted name (`"label"`) is read as one; any other quoted
            // text is read as code, so no definition hides in a string the
            // scan misreads.
            let close = chars[i + 1..].iter().position(|q| *q == c);
            let inner: Option<String> = close.map(|p| chars[i + 1..i + 1 + p].iter().collect());
            match (close, inner) {
                (Some(p), Some(inner))
                    if !inner.is_empty()
                        && inner
                            .chars()
                            .all(|ch| is_word_char(ch) || "?!=".contains(ch)) =>
                {
                    out.push(Token::Quoted(inner));
                    i += p + 2;
                }
                _ => i += 1,
            }
        } else {
            if !c.is_whitespace() {
                out.push(Token::Punct(c));
            }
            i += 1;
        }
    }
    out
}

fn word(token: Option<&Token>) -> Option<&str> {
    match token {
        Some(Token::Word(w)) => Some(w),
        _ => None,
    }
}

fn is_constant(word: &str) -> bool {
    word.chars().next().is_some_and(char::is_uppercase)
}

/// The constant path starting at `tokens[at]` (`A`, `A::B`, `::A`): its
/// last segment and the index past it.
fn constant_path(tokens: &[Token], mut at: usize) -> Option<(String, usize)> {
    let mut last = None;
    loop {
        while tokens.get(at) == Some(&Token::Punct(':')) {
            at += 1;
        }
        match word(tokens.get(at)) {
            Some(segment) if is_constant(segment) => {
                last = Some(segment.to_owned());
                at += 1;
            }
            _ => break,
        }
        if tokens.get(at) != Some(&Token::Punct(':')) {
            break;
        }
    }
    last.map(|last| (last, at))
}

fn read_statement(tokens: &[Token], unread: &mut Unread) {
    for (i, token) in tokens.iter().enumerate() {
        let Token::Word(keyword) = token else {
            continue;
        };
        // A word after `.` is a method call, not a keyword.
        let called = i > 0 && tokens[i - 1] == Token::Punct('.');
        match keyword.as_str() {
            "def" if !called => read_def(&tokens[i + 1..], unread),
            "class" | "module" if !called => {
                let singleton = tokens.get(i + 1) == Some(&Token::Punct('<'))
                    && tokens.get(i + 2) == Some(&Token::Punct('<'));
                let at = if singleton { i + 3 } else { i + 1 };
                if let Some((last, _)) = constant_path(tokens, at) {
                    unread.owners.push(last.clone());
                    if !singleton {
                        unread.constants.push(last);
                    }
                }
            }
            "include" | "extend" | "prepend" if !called => unread.any = true,
            "alias" | "undef" if !called => {
                for name in tokens[i + 1..].iter().take(2) {
                    match name {
                        Token::Word(name) | Token::Symbol(name) => {
                            unread.methods.push(name.clone());
                        }
                        _ => {}
                    }
                }
            }
            declaration if DECLARATIONS.contains(&declaration) => {
                let mut literal = false;
                for name in &tokens[i + 1..] {
                    if let Token::Symbol(name) | Token::Quoted(name) = name {
                        literal = true;
                        unread.methods.push(name.clone());
                        unread.methods.push(format!("{name}="));
                    }
                }
                // `attr_reader(*FIELDS)`, `define_method(name)`: any name.
                let computed = !literal && tokens.len() > i + 1;
                let prefixed = declaration == "delegate"
                    && tokens[i + 1..]
                        .iter()
                        .any(|t| matches!(t, Token::Word(w) if w == "prefix"));
                unread.any |= computed || prefixed;
            }
            // `A = 1`, `A::B ||= 2`, read from the first segment of the
            // path wherever it stands.
            _ if is_constant(keyword)
                && !called
                && (i == 0 || tokens[i - 1] != Token::Punct(':')) =>
            {
                if let Some((last, after)) = constant_path(tokens, i)
                    && assigns(&tokens[after..])
                {
                    unread.constants.push(last);
                }
            }
            _ => {}
        }
    }
}

/// `= v` or `||= v`, not `==`, `=~` or `=>`.
fn assigns(rest: &[Token]) -> bool {
    let rest: Vec<char> = rest
        .iter()
        .take(4)
        .map_while(|t| match t {
            Token::Punct(c) => Some(*c),
            _ => None,
        })
        .collect();
    match rest.as_slice() {
        ['=', next, ..] => !matches!(next, '=' | '~' | '>'),
        ['=', ..] => true,
        ['|', '|', '=', ..] => true,
        _ => false,
    }
}

/// `def m`, `def self.m`, `def X.m`, `def m=(v)`, `def ==(o)`.
fn read_def(rest: &[Token], unread: &mut Unread) {
    let (receiver, name_at) = match (rest.first(), rest.get(1)) {
        (Some(Token::Word(receiver)), Some(Token::Punct('.'))) => (Some(receiver.as_str()), 2),
        _ => (None, 0),
    };
    if let Some(receiver) = receiver.filter(|r| is_constant(r)) {
        unread.owners.push(receiver.to_owned());
    }
    let name = match rest.get(name_at) {
        Some(Token::Word(name)) => {
            // A setter: `def name=(v)`; an endless def has a space first,
            // which the tokens do not keep, so both are listed.
            if rest.get(name_at + 1) == Some(&Token::Punct('=')) {
                unread.methods.push(format!("{name}="));
            }
            name.clone()
        }
        Some(Token::Punct(_)) => rest[name_at..]
            .iter()
            .map_while(|t| match t {
                Token::Punct(c) if *c != '(' => Some(*c),
                _ => None,
            })
            .collect(),
        _ => return,
    };
    if !name.is_empty() {
        unread.methods.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Every way a region defines a name is read, in code that no grammar
    /// accepts; nothing is lost to a comment marker after code, a string or
    /// an unbalanced bracket.
    #[test]
    fn c10_a_scan_lists_every_name_a_region_may_define() {
        // D12
        let unread = scan(
            "module Admin::Tools
  class User < Base
    LIMIT = 10
    Admin::Other ||= 2
    def self.find(&) = lookup(1, &
    def name=(v); @n = v; end
    def ==(other) other end
    def User.build; end
    attr_accessor :token, \"label\"
    alias_method :old_find, :find
    alias fresh stale
    # def commented_out; end
    x == Y
    z = W
    if a then B = 3 end
    note(%q(it's)) && def after_quote; end
  end
  class << Registry
    def entry; end
  end
end
",
        );
        assert_eq!(
            unread.methods,
            names(&[
                "==",
                "after_quote",
                "build",
                "entry",
                "find",
                "find=",
                "fresh",
                "label",
                "label=",
                "name",
                "name=",
                "old_find",
                "old_find=",
                "stale",
                "token",
                "token=",
            ])
        );
        assert_eq!(unread.owners, names(&["Registry", "Tools", "User"]));
        assert_eq!(
            unread.constants,
            names(&["B", "LIMIT", "Other", "Tools", "User"])
        );
        assert!(!unread.any);
    }

    /// An ancestor added or a method named at run time may define any
    /// name.
    #[test]
    fn c10_a_scan_marks_ancestors_and_computed_names_as_any_name() {
        // D12
        for text in [
            "class A\n  include Helpers\nend\n",
            "class A\n  define_method(name) { 1 }\nend\n",
            "class A\n  delegate :x, to: :y, prefix: true\nend\n",
            "class A\n  attr_accessor *FIELDS\nend\n",
        ] {
            assert!(scan(text).any, "{text}");
        }
        assert!(!scan("class A\n  define_method(:x) { 1 }\nend\n").any);
    }

    /// A definition the scan cannot place on an owner it names is loose:
    /// in a block run elsewhere, on another object, in `class << obj`;
    /// one in a class body, an iterator, `included do`, a method or a
    /// block evaluated on a named class is not. Strings, comments and
    /// heredocs open and close nothing.
    #[test]
    fn c10_a_scan_tells_a_definition_it_cannot_place() {
        // B9
        let unread = scan(
            r##"class Helper
  def render(x, &)
    tag.div class: "end do", data: { a: 1 } do
      link_to x, &
    end
  end
  # end do def nope
  NOTE = <<~TEXT
    end end do
  TEXT
  [:a].each { |n| define_method(:each_one) { n } }
  def wrap
    x = if y then 1 else 2 end
    z = "#{w.map { |v| v }} end"
    z
  end
end
Base.prepend(Module.new { def hook; end })
Other.class_eval do
  def evaluated; end
end
widgets.each do |w|
  w.define_method(:sent) { 1 }
end
describe "x" do
  def helper; end
end
class << object
  def single; end
end
def obj.on_object; end
"##,
        );
        assert_eq!(
            unread.loose,
            names(&["helper", "hook", "on_object", "sent", "single"])
        );
        assert!(!unread.loose_any);
        assert!(unread.any);
        assert_eq!(unread.owners, names(&["Base", "Helper", "Other"]));
        assert!(
            !scan("class A\n  def a; end\n  included do\n    def b; end\n  end\nend\n").loose_any
        );
        assert!(scan("foo do\n  klass.include(M)\nend\n").loose_any);
        assert!(
            scan("module M\n  def self.included(base)\n    base.extend(C)\n  end\nend\n")
                .loose
                .is_empty()
        );
    }

    // R5-01: a pattern of many interpolations is matched in one pass,
    // with no recursion that a long one could exhaust the stack with.
    #[test]
    fn a_pattern_fits_each_name_its_interpolations_may_spell() {
        let i = INTERPOLATED;
        for (pattern, name, fits_it) in [
            ("run".to_owned(), "run", true),
            ("run".to_owned(), "runs", false),
            (format!("_run_{i}_callbacks"), "_run_save_callbacks", true),
            (format!("_run_{i}_callbacks"), "_run__callbacks", true),
            (format!("_run_{i}_callbacks"), "_run_callbacks", false),
            (format!("{i}a{i}a"), "aa", true),
            (format!("{i}a{i}a"), "a", false),
            (format!("x{i}"), "x", true),
            (format!("{i}"), "", true),
            (format!("a{i}b{i}c"), "abbc", true),
            (format!("a{i}b{i}c"), "acb", false),
        ] {
            assert_eq!(fits(&pattern, name), fits_it, "{pattern:?} {name}");
        }
        let long = format!("{i}a").repeat(100_000);
        assert!(fits(&long, &"a".repeat(100_000)));
        assert!(!fits(&long, &"a".repeat(99_999)));
    }

    #[test]
    fn a_scan_round_trips_through_its_record() {
        let unread = scan("class A\n  include B\n  def a; end\n  C = 1\nend\n");
        let record = unread.record();
        assert_eq!(record[0], "unread");
        assert_eq!(Unread::from_record(&record[1..]), unread);
    }
}
