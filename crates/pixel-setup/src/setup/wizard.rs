// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The questions `pixel setup` asks a person at a terminal: one per agent, with
//! the agents it finds in use (and the one running the wizard) answered yes by
//! default. The driver supplies the streams, so the dialogue is testable and
//! `pixel setup --agent …` never reaches it.

use std::io::{self, BufRead, Write};
use std::path::Path;

use super::{AgentTarget, Scope};

/// How many times one question is asked before its default stands.
const ATTEMPTS: usize = 3;

/// A reply read as yes or no; `None` when it is neither (an empty reply is
/// the default's, decided by the caller).
fn answer(reply: &str) -> Option<bool> {
    match reply.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Asks one question and returns the answer. An empty reply, an end of input
/// and a third unreadable reply all answer `default`.
/// Asks one yes/no question and returns the answer.
///
/// # Errors
///
/// A failed read or write.
pub fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
    default: bool,
) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    for _ in 0..ATTEMPTS {
        write!(output, "{question} {hint} ")?;
        output.flush()?;
        let mut reply = String::new();
        if input.read_line(&mut reply)? == 0 {
            writeln!(output)?;
            return Ok(default);
        }
        if reply.trim().is_empty() {
            return Ok(default);
        }
        if let Some(yes) = answer(&reply) {
            return Ok(yes);
        }
        writeln!(output, "  answer y or n (Enter keeps {hint})")?;
    }
    Ok(default)
}

/// Walks [`AgentTarget::ALL`] and returns the agents the person chose. An
/// agent is a default yes when its configuration exists under `home` or when
/// it is `running` the wizard.
///
/// # Errors
///
/// A failed read from `input` or write to `output`.
pub fn choose(
    input: &mut impl BufRead,
    output: &mut impl Write,
    home: &Path,
    running: Option<AgentTarget>,
) -> io::Result<Vec<AgentTarget>> {
    writeln!(
        output,
        "pixel setup: choose the agents to set up (Enter keeps the default)."
    )?;
    let mut chosen = Vec::new();
    for agent in AgentTarget::ALL {
        let default = running == Some(agent) || agent.in_use(home, Scope::Global);
        let question = format!("{}: {}\n  Set up?", agent.name(), agent.help());
        if ask(input, output, &question, default)? {
            chosen.push(agent);
        }
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests;
