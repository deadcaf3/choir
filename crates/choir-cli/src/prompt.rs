//! The one place in this binary that may ask a person a question.
//!
//! Everything else here must complete without a terminal, because an
//! agent driving this CLI has none and a prompt hangs it with no error
//! and nothing in a log — that is the rule `no_tty_block.rs` asserts,
//! and this module is its single named exemption.
//!
//! What earns the exemption is the shape of [`ask`], not the question
//! it happens to carry. It never blocks: with no terminal on stdin it
//! returns `None` immediately, and every caller is obliged to turn that
//! into a refusal naming the flag that supplies the answer. So the
//! branch a source scan is really looking for — "wait forever for input
//! that is not coming" — does not exist here, and cannot be added
//! without deleting the first line of the function.
//!
//! One question is asked today: the account name (D75). An invite that
//! leaves the seat open is the ordinary kind, and that name is the one
//! string about a person the op log can never withdraw, so defaulting it
//! from `$USER` or the invite id would be picking it on their behalf and
//! calling it a convenience.
//!
//! # Examples
//!
//! ```
//! // Under `cargo test` stdin is not a terminal, so this never blocks.
//! assert_eq!(choir_cli::prompt::ask("ignored"), None);
//! ```

use std::io::{IsTerminal, Write};

/// Asks `question` on stderr and reads one non-empty line from stdin.
///
/// `None` when there is no terminal to ask, or when stdin reaches end of
/// file. The question goes to stderr so that a caller's own answer stays
/// the only thing on stdout.
#[must_use]
pub fn ask(question: &str) -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    loop {
        eprint!("{question} ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).ok()? == 0 {
            return None;
        }
        let answer = line.trim().to_string();
        if !answer.is_empty() {
            return Some(answer);
        }
    }
}
