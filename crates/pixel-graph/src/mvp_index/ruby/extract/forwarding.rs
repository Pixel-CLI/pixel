// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What tree-sitter-ruby 0.23.1, the latest, cannot parse of Ruby 3.1's
//! anonymous forwarding (#107), read over the file's own bytes.

use tree_sitter::{Node, Tree};

use super::PARSE_BUDGET;

/// The tree of `content`, read past the anonymous forwarding
/// tree-sitter-ruby 0.23.1 (the latest) cannot parse (#107): a bare `&`
/// or `**` ending the arguments of a call without parentheses (`tag.div
/// id: x, &`, Ruby 3.1+). Its error recovery then loses the `end` after
/// it, and with it the class or module around every `def` of the file.
/// Each such argument is parsed as `&1` (`**1`), the blank before it
/// moved after it, which keeps every byte offset: the tree of the
/// rewritten text is walked over the file's own text, so no name, line or
/// signature changes, and a block passed on is still one (`flow`). The
/// rewrite is kept only when it leaves fewer errors.
pub(super) fn parse(content: &[u8]) -> Option<Tree> {
    let tree = super::parse(content, PARSE_BUDGET)?;
    if !tree.root_node().has_error() {
        return Some(tree);
    }
    let Some(rewritten) = forwarded(content) else {
        return Some(tree);
    };
    match super::parse(&rewritten, PARSE_BUDGET) {
        Some(better) if errors(better.root_node()) < errors(tree.root_node()) => Some(better),
        _ => Some(tree),
    }
}

/// `content` with each bare `&` or `**` that ends the arguments of a call
/// without parentheses, after a comma (on its line or the line above) and
/// a blank, written `&1` (`**1`) one
/// byte earlier; `None` when it has none. One with no blank before it
/// (`f 1,&`, or at the start of a line) leaves no byte to spare and stays
/// unread: a known residual.
fn forwarded(content: &[u8]) -> Option<Vec<u8>> {
    let blank = |b: u8| b == b' ' || b == b'\t';
    let mut out = content.to_vec();
    let mut changed = false;
    let mut i = 1;
    while i < content.len() {
        let width = if content[i] == b'&' {
            1
        } else if content[i..].starts_with(b"**") {
            2
        } else {
            i += 1;
            continue;
        };
        let end = i + width;
        // A blank before it, a comma before that, maybe ending the line
        // above (`link_to url,\n  &`).
        let comma = blank(content[i - 1])
            && content[..i - 1]
                .iter()
                .rev()
                .find(|&&b| !blank(b) && b != b'\n' && b != b'\r')
                .is_some_and(|&b| b == b',');
        // Nothing after it on the line but blanks and a comment, or a `;`.
        let ends = content[end..]
            .iter()
            .find(|&&b| !blank(b))
            .is_none_or(|&b| matches!(b, b'\n' | b'\r' | b'#' | b';'));
        let touching = content.get(end).is_some_and(|&b| b == b'&' || b == b'*');
        if comma && ends && !touching {
            out.copy_within(i..end, i - 1);
            out[end - 1] = b'1';
            changed = true;
        }
        i = end;
    }
    changed.then_some(out)
}

/// How many nodes of the tree under `root` are errors or missing; a
/// subtree without one is not entered.
fn errors(root: Node) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        count += usize::from(node.is_error() || node.is_missing());
        let mut children = node.walk();
        stack.extend(
            node.children(&mut children)
                .filter(|child| child.has_error() || child.is_missing()),
        );
    }
    count
}
