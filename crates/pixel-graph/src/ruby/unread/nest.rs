// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Where an unread region's definitions land, from the nesting of its
//! words (B9). A `def` or a declaration written in a class or module body,
//! or in a block Ruby runs there (an iterator, `included do`, a
//! `class_eval` on the class itself), defines on an owner the scan names,
//! or on one whose lookup passes through it; one written in any other
//! block (`Module.new { def m }`, a DSL's), on another object (`def obj.m`,
//! `klass.define_method(:m)`) or in `class << obj` may land on any class or
//! module: its name is loose. A constant receiver of a definer or a mixin
//! (`Base.prepend(..)`, `Base.class_eval do`) is an owner the region may
//! reopen.
//!
//! The pass reads the region with string, regexp, heredoc and comment
//! contents blanked, so a word there opens or closes nothing; where it
//! cannot tell a frame, it opens one, so a block is never closed early.

use super::{DECLARATIONS, Token, Unread, is_constant, tokens, word};

/// The kinds of a frame the nesting opens.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Frame {
    /// A class or module body, or a block evaluated on a class the scan
    /// names: what it defines is that owner's.
    Owner,
    /// A `def` body, with a hook's parameter (`self.included(base)`):
    /// what the hook does to `base` the hook's name stands for.
    Def(Option<String>),
    /// A block that may run on an object the scan cannot name, or `class
    /// << x` for an expression `x`.
    Loose,
    /// Any other construct `end` or `}` closes, a block run where it is.
    Other,
}

/// The methods whose block Ruby runs on the object around it, or on the
/// class including the concern, whose lookup passes through the owner.
const KEEPS: &[&str] = &[
    "each",
    "each_with_index",
    "times",
    "map",
    "included",
    "prepended",
    "with_options",
];

/// The methods that evaluate a block on their receiver.
const EVALUATORS: &[&str] = &[
    "class_eval",
    "class_exec",
    "module_eval",
    "module_exec",
    "instance_eval",
    "instance_exec",
];

/// The definers and mixins whose receiver decides where they act.
fn is_definer(word: &str) -> bool {
    DECLARATIONS.contains(&word)
        || EVALUATORS.contains(&word)
        || matches!(word, "include" | "extend" | "prepend")
}

/// The receiver of a call whose method is at `at` (after a `.`).
enum Receiver {
    /// `self`, `singleton_class`, `self.class`: the class whose body or
    /// method this is.
    Own,
    /// A constant path: its last segment.
    Constant(String),
    /// Anything else: a local's or a method's value, by its last word.
    Other(String),
}

fn receiver(tokens: &[Token], at: usize) -> Receiver {
    let Some(before) = at.checked_sub(2) else {
        return Receiver::Other(String::new());
    };
    match word(tokens.get(before)) {
        Some("self" | "singleton_class") => Receiver::Own,
        Some("class") if before >= 2 && word(tokens.get(before - 2)) == Some("self") => {
            Receiver::Own
        }
        Some(w) if is_constant(w) => Receiver::Constant(w.to_owned()),
        Some(w) => Receiver::Other(w.to_owned()),
        None => Receiver::Other(String::new()),
    }
}

/// Reads the frames of `text` and records in `unread` what it defines
/// where the scan cannot place it.
pub(super) fn read(text: &str, unread: &mut Unread) {
    let code = blank(text);
    let mut stack: Vec<Frame> = Vec::new();
    for line in code.lines() {
        for statement in line.split(';') {
            read_statement(&tokens(statement), &mut stack, unread);
        }
    }
}

/// Whether what is written here may land on an object the scan cannot
/// name: a loose frame stands between it and the nearest owner.
fn loose(stack: &[Frame]) -> bool {
    stack
        .iter()
        .rev()
        .take_while(|frame| **frame != Frame::Owner)
        .any(|frame| *frame == Frame::Loose)
}

/// The parameter of the hook whose body the walk is in, if any.
fn hook(stack: &[Frame]) -> Option<&str> {
    stack.iter().rev().find_map(|frame| match frame {
        Frame::Def(param) => Some(param.as_deref()),
        _ => None,
    })?
}

/// The literal names after `at`, or `None` when one is computed.
fn literal_names(tokens: &[Token]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for token in tokens {
        match token {
            Token::Symbol(name) | Token::Quoted(name) => names.push(name.clone()),
            Token::Punct('*') => return None,
            Token::Word(w) if !is_constant(w) && !matches!(w.as_str(), "to" | "prefix") => {
                return None;
            }
            _ => {}
        }
    }
    Some(names)
}

fn read_statement(tokens: &[Token], stack: &mut Vec<Frame>, unread: &mut Unread) {
    for (i, token) in tokens.iter().enumerate() {
        let called = i > 0 && tokens[i - 1] == Token::Punct('.');
        // `class: "btn"`, `if: :ready?`: a label, not a keyword.
        let label = tokens.get(i + 1) == Some(&Token::Punct(':'))
            && tokens.get(i + 2) != Some(&Token::Punct(':'));
        match token {
            Token::Punct('{') => {
                let block = matches!(
                    i.checked_sub(1).and_then(|p| tokens.get(p)),
                    Some(Token::Punct(')')) | Some(Token::Word(_))
                );
                stack.push(if block {
                    block_frame(tokens, i)
                } else {
                    Frame::Other
                });
            }
            Token::Punct('}') => {
                stack.pop();
            }
            Token::Word(w) if !called && !label => match w.as_str() {
                "def" => {
                    def(&tokens[i + 1..], stack, unread);
                }
                "class" => {
                    let singleton = tokens.get(i + 1) == Some(&Token::Punct('<'))
                        && tokens.get(i + 2) == Some(&Token::Punct('<'));
                    let frame = match word(tokens.get(i + 3)) {
                        _ if !singleton => Frame::Owner,
                        Some("self") => Frame::Other,
                        Some(c) if is_constant(c) => Frame::Owner,
                        _ => Frame::Loose,
                    };
                    stack.push(frame);
                }
                "module" => stack.push(Frame::Owner),
                "do" => stack.push(block_frame(tokens, i)),
                "begin" | "case" => stack.push(Frame::Other),
                "if" | "unless" | "while" | "until" | "for" if starts(tokens, i) => {
                    stack.push(Frame::Other)
                }
                "end" => {
                    stack.pop();
                }
                "include" | "extend" | "prepend" if loose(stack) => unread.loose_any = true,
                declaration if DECLARATIONS.contains(&declaration) && loose(stack) => {
                    match literal_names(&tokens[i + 1..]) {
                        Some(names) => unread.loose.extend(names),
                        None => unread.loose_any = true,
                    }
                }
                _ => {}
            },
            // A definer or a mixin sent to a receiver.
            Token::Word(w) if called && is_definer(w) => match receiver(tokens, i) {
                Receiver::Own if loose(stack) => match literal_names(&tokens[i + 1..]) {
                    Some(names) if !matches!(w.as_str(), "include" | "extend" | "prepend") => {
                        unread.loose.extend(names)
                    }
                    _ => unread.loose_any = true,
                },
                Receiver::Own => {}
                Receiver::Constant(owner) => {
                    unread.owners.push(owner);
                    unread.any = true;
                }
                // What a hook does to its `base` the hook's name stands for.
                Receiver::Other(object) if hook(stack) == Some(object.as_str()) => {}
                Receiver::Other(_) => match literal_names(&tokens[i + 1..]) {
                    Some(names)
                        if DECLARATIONS.contains(&w.as_str())
                            && !matches!(w.as_str(), "include" | "extend" | "prepend") =>
                    {
                        unread.loose.extend(names)
                    }
                    _ => unread.loose_any = true,
                },
            },
            _ => {}
        }
    }
}

/// Whether `tokens[at]` starts a statement or an expression: an `if`
/// there opens a frame, after a value it is a modifier.
fn starts(tokens: &[Token], at: usize) -> bool {
    match at.checked_sub(1).map(|p| &tokens[p]) {
        None => true,
        Some(Token::Punct(c)) => matches!(c, '=' | '(' | '[' | ',' | '{' | '|' | '&' | '!'),
        Some(Token::Word(w)) => matches!(w.as_str(), "then" | "else" | "do" | "and" | "or" | "not"),
        _ => false,
    }
}

/// `def m`, `def self.m`, `def X.m`, `def obj.m`: a frame unless endless,
/// and a loose name when it may land on an object the scan cannot name.
fn def(rest: &[Token], stack: &mut Vec<Frame>, unread: &mut Unread) {
    let receiver = match (rest.first(), rest.get(1)) {
        (Some(Token::Word(object)), Some(Token::Punct('.'))) => Some(object.as_str()),
        _ => None,
    };
    let name_at = if receiver.is_some() { 2 } else { 0 };
    let name = word(rest.get(name_at)).map(str::to_owned);
    let elsewhere = receiver.is_some_and(|r| r != "self" && !is_constant(r));
    if let Some(name) = &name
        && (elsewhere || (receiver.is_none_or(|r| r == "self") && loose(stack)))
    {
        unread.loose.push(name.clone());
    }
    // `def m = expr` and `def m(a) = expr` have no `end`.
    let mut after = name_at + 1;
    if rest.get(after) == Some(&Token::Punct('(')) {
        let mut depth = 0;
        for (j, token) in rest.iter().enumerate().skip(after) {
            match token {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        after = j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
    } else if rest.get(after) == Some(&Token::Punct('='))
        && rest.get(after + 1) == Some(&Token::Punct('('))
    {
        // A setter, `def name=(v)`.
        after = rest.len();
    }
    let endless = name.is_some()
        && rest.get(after) == Some(&Token::Punct('='))
        && !matches!(rest.get(after + 1), Some(Token::Punct('=' | '~' | '>')));
    if endless {
        return;
    }
    // A hook's parameter: `def self.included(base)`.
    let param = match (receiver, name.as_deref()) {
        (Some("self"), Some("included" | "extended" | "prepended" | "inherited")) => {
            match (
                rest.get(name_at + 1),
                rest.get(name_at + 2),
                rest.get(name_at + 3),
            ) {
                (Some(Token::Punct('(')), Some(Token::Word(param)), Some(Token::Punct(')'))) => {
                    Some(param.clone())
                }
                _ => None,
            }
        }
        _ => None,
    };
    stack.push(Frame::Def(param));
}

/// The frame of the block opened at `at` (`do` or `{`), from the call it
/// is passed to: the nearest method called before it with a `.`, else the
/// statement's first word.
fn block_frame(tokens: &[Token], at: usize) -> Frame {
    // `X = Class.new(..) do`, `X = Struct.new do`: the class `X`.
    let assigned = word(tokens.first()).is_some_and(is_constant)
        && tokens.get(1) == Some(&Token::Punct('='))
        && tokens.get(2) != Some(&Token::Punct('='));
    let head = (0..at)
        .rev()
        .find(|&j| j > 0 && tokens[j - 1] == Token::Punct('.') && word(tokens.get(j)).is_some())
        .or_else(|| word(tokens.first()).map(|_| 0));
    let Some(head) = head else {
        return Frame::Loose;
    };
    let method = word(tokens.get(head)).unwrap_or_default();
    let on = (head > 0).then(|| receiver(tokens, head));
    match (method, on) {
        ("new", Some(Receiver::Constant(c)))
            if matches!(c.as_str(), "Class" | "Module" | "Struct") =>
        {
            if assigned {
                Frame::Owner
            } else {
                Frame::Loose
            }
        }
        ("define", Some(Receiver::Constant(c))) if c == "Data" => {
            if assigned {
                Frame::Owner
            } else {
                Frame::Loose
            }
        }
        (method, None | Some(Receiver::Own)) if EVALUATORS.contains(&method) => Frame::Other,
        (method, Some(Receiver::Constant(_))) if EVALUATORS.contains(&method) => Frame::Owner,
        (method, _) if KEEPS.contains(&method) => Frame::Other,
        ("class_methods" | "concerning", None) => Frame::Owner,
        _ => Frame::Loose,
    }
}

/// What closes the literal [`blank`] is in, innermost last: a quote, a
/// percent literal's delimiter with its depth, or an interpolation's brace
/// depth.
enum Mode {
    Code {
        braces: usize,
    },
    Quote(char),
    Percent {
        open: char,
        close: char,
        depth: usize,
    },
    Regexp,
    Comment,
}

/// `text` with the contents of strings, regexps, heredocs and comments
/// replaced by spaces, line breaks kept.
fn blank(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut modes = vec![Mode::Code { braces: 0 }];
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut in_heredoc: Option<(String, bool)> = None;
    let mut i = 0;
    let operand = |out: &str| {
        let last = out.trim_end().chars().last();
        last.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ')' | ']' | '}'))
    };
    while i < chars.len() {
        let c = chars[i];
        if let Some((end, squiggly)) = &in_heredoc {
            let line_end = chars[i..]
                .iter()
                .position(|ch| *ch == '\n')
                .map_or(chars.len(), |p| i + p);
            let line: String = chars[i..line_end].iter().collect();
            let done = if *squiggly {
                line.trim() == end
            } else {
                line == *end
            };
            if done {
                in_heredoc = None;
                out.push_str(&line);
            } else {
                out.extend(line.chars().map(|_| ' '));
            }
            i = line_end;
            if i < chars.len() {
                out.push('\n');
                i += 1;
            }
            continue;
        }
        if c == '\n' {
            out.push('\n');
            if matches!(modes.last(), Some(Mode::Comment)) {
                modes.pop();
            }
            if matches!(modes.last(), Some(Mode::Code { .. })) && !heredocs.is_empty() {
                in_heredoc = Some(heredocs.remove(0));
            }
            i += 1;
            // `=begin` to `=end`: a comment.
            if modes.len() == 1
                && in_heredoc.is_none()
                && chars[i..].starts_with(&['=', 'b', 'e', 'g', 'i', 'n'])
            {
                in_heredoc = Some(("=end".to_owned(), false));
                let line_end = chars[i..]
                    .iter()
                    .position(|ch| *ch == '\n')
                    .map_or(chars.len(), |p| i + p);
                out.extend(chars[i..line_end].iter().map(|_| ' '));
                i = line_end;
            }
            continue;
        }
        let blanked = |out: &mut String| out.push(if c == '\n' { '\n' } else { ' ' });
        let interpolating = modes.len() > 1;
        match modes.last_mut() {
            Some(Mode::Comment) => blanked(&mut out),
            Some(Mode::Quote(q)) => {
                let q = *q;
                if c == '\\' {
                    out.push(' ');
                    if i + 1 < chars.len() && chars[i + 1] != '\n' {
                        out.push(' ');
                        i += 1;
                    }
                } else if c == q {
                    modes.pop();
                    out.push(c);
                } else if c == '#' && q != '\'' && chars.get(i + 1) == Some(&'{') {
                    modes.push(Mode::Code { braces: 1 });
                    out.push_str("  ");
                    i += 1;
                } else {
                    blanked(&mut out);
                }
            }
            Some(Mode::Percent { open, close, depth }) => {
                if c == '\\' {
                    out.push(' ');
                    if i + 1 < chars.len() && chars[i + 1] != '\n' {
                        out.push(' ');
                        i += 1;
                    }
                } else if c == *close && *depth == 0 {
                    modes.pop();
                    out.push(c);
                } else {
                    if c == *close {
                        *depth -= 1;
                    } else if c == *open && open != close {
                        *depth += 1;
                    }
                    blanked(&mut out);
                }
            }
            Some(Mode::Regexp) => {
                if c == '\\' {
                    out.push(' ');
                    if i + 1 < chars.len() && chars[i + 1] != '\n' {
                        out.push(' ');
                        i += 1;
                    }
                } else if c == '/' {
                    modes.pop();
                    out.push(c);
                } else {
                    blanked(&mut out);
                }
            }
            Some(Mode::Code { braces }) => {
                match c {
                    '#' => {
                        modes.push(Mode::Comment);
                        out.push(' ');
                    }
                    '\'' | '"' | '`' => {
                        modes.push(Mode::Quote(c));
                        out.push(c);
                    }
                    '{' if interpolating => {
                        *braces += 1;
                        out.push(c);
                    }
                    '}' if interpolating => {
                        *braces -= 1;
                        if *braces == 0 {
                            modes.pop();
                            out.push(' ');
                        } else {
                            out.push(c);
                        }
                    }
                    '%' if !operand(&out) => {
                        let mut j = i + 1;
                        if chars.get(j).is_some_and(|k| "qQwWiIrsx".contains(*k)) {
                            j += 1;
                        }
                        match chars.get(j) {
                            Some(&open) if "([{<|!/^".contains(open) => {
                                let close = match open {
                                    '(' => ')',
                                    '[' => ']',
                                    '{' => '}',
                                    '<' => '>',
                                    other => other,
                                };
                                modes.push(Mode::Percent {
                                    open,
                                    close,
                                    depth: 0,
                                });
                                out.extend(chars[i..=j].iter());
                                i = j;
                            }
                            _ => out.push(c),
                        }
                    }
                    '/' if !operand(&out) => {
                        modes.push(Mode::Regexp);
                        out.push(c);
                    }
                    '<' if chars.get(i + 1) == Some(&'<') => {
                        // `<<~EOS`, `<<-EOS`, `<<EOS`, maybe quoted.
                        let mut j = i + 2;
                        let squiggly = matches!(chars.get(j), Some('~' | '-'));
                        if squiggly {
                            j += 1;
                        }
                        let quoted = matches!(chars.get(j), Some('\'' | '"' | '`'));
                        if quoted {
                            j += 1;
                        }
                        let start = j;
                        while chars
                            .get(j)
                            .is_some_and(|k| k.is_alphanumeric() || *k == '_')
                        {
                            j += 1;
                        }
                        let id: String = chars[start..j].iter().collect();
                        let upper = id
                            .chars()
                            .next()
                            .is_some_and(|k| k.is_uppercase() || k == '_');
                        if !id.is_empty() && (squiggly || quoted || (upper && !operand(&out))) {
                            if quoted {
                                j += 1;
                            }
                            heredocs.push((id, squiggly));
                            out.extend(chars[i..j.min(chars.len())].iter());
                            i = j.min(chars.len());
                            continue;
                        }
                        out.push(c);
                    }
                    '?' if chars.get(i + 1).is_some_and(|k| !k.is_whitespace())
                        && !operand(&out) =>
                    {
                        // A character literal, `?a`.
                        out.push_str("  ");
                        i += 1;
                    }
                    _ => out.push(c),
                }
            }
            None => out.push(c),
        }
        i += 1;
    }
    out
}
