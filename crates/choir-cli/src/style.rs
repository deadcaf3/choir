//! Terminal styling for the parts of `choir` a person reads (D58).
//!
//! Two rules hold this together, and they are what make colour safe in a
//! tool whose other half is an agent:
//!
//! 1. **Data goes to stdout, unstyled, always.** Every command that
//!    answers with JSON answers with exactly the bytes the node sent.
//!    Nothing in here is ever applied to that stream, so a pipeline sees
//!    the same thing whether or not a terminal is attached.
//! 2. **Diagnostics go to stderr, styled only when a person is looking.**
//!    A person is looking when stderr is a terminal, `NO_COLOR` is unset,
//!    and `TERM` is not `dumb`.
//!
//! Hand-rolled rather than `owo-colors` or `console` for the reason the
//! rest of the workspace hand-rolls: this is four escape sequences and a
//! predicate, and the two dependencies it would replace pull in
//! terminal-detection stacks whose behaviour we would then have to test
//! anyway.
//!
//! # Examples
//!
//! ```
//! let plain = choir_cli::style::Style::plain();
//! assert_eq!(plain.bold("choir"), "choir");
//! ```

use std::io::IsTerminal;

/// Whether styled output is wanted, and the sequences for it.
///
/// Construct with [`Style::for_stderr`] at the point of use rather than
/// caching one: the decision is cheap, and a cached one taken before a
/// stream was redirected would be wrong.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    colour: bool,
}

/// `NO_COLOR` and `TERM` are read here and nowhere else.
///
/// The workspace rule is that no crate takes *configuration* from the
/// environment. These are not configuration: they carry no setting of
/// ours, they change no behaviour a script can observe, and there is
/// deliberately no `--color` flag for them to be a shortcut around. They
/// describe the terminal, which is the one thing a flag cannot know.
fn a_person_is_looking(stream_is_terminal: bool) -> bool {
    if !stream_is_terminal {
        return false;
    }
    // Any value at all disables colour, per the NO_COLOR convention; an
    // empty value does not, since that is how a variable is unset in
    // shells that cannot unset it.
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

impl Style {
    /// The style for diagnostics: colour only when stderr is a terminal.
    #[must_use]
    pub fn for_stderr() -> Self {
        Self {
            colour: a_person_is_looking(std::io::stderr().is_terminal()),
        }
    }

    /// The style for help text, which is printed to stdout when it was
    /// asked for and to stderr when it is a refusal.
    #[must_use]
    pub fn for_stdout() -> Self {
        Self {
            colour: a_person_is_looking(std::io::stdout().is_terminal()),
        }
    }

    /// A style that emits no escape sequences, whatever is attached.
    #[must_use]
    pub fn plain() -> Self {
        Self { colour: false }
    }

    /// Whether anything this style produces will differ from plain text.
    ///
    /// Read by callers whose whole output is decoration: a summary that
    /// nobody is looking at is not worth the two lines it would push a
    /// real error off the top of the screen with.
    #[must_use]
    pub fn is_painted(self) -> bool {
        self.colour
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.colour {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// The one word on a line that carries its meaning.
    #[must_use]
    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Context that must not compete with what it qualifies.
    #[must_use]
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    /// A refusal.
    #[must_use]
    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    /// An acceptance.
    #[must_use]
    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    /// Something to type.
    #[must_use]
    pub fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }
}

/// Levenshtein distance between two ASCII-ish words, for "did you mean".
///
/// Two rows rather than a full matrix, because the only caller compares
/// one typo against thirty-two short names and a full matrix would be
/// more code for the same answer.
#[must_use]
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitute.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The closest command name to `typed`, when one is close enough to be
/// worth suggesting.
///
/// The threshold is deliberately tight. A suggestion that is wrong is
/// worse than none: the reader types it, gets a second refusal, and now
/// distrusts the first one. One third of the typed length, so `revieww`
/// suggests `review` and `deploy` suggests nothing at all.
#[must_use]
pub fn nearest<'a>(typed: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let budget = (typed.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;
    for name in names {
        // A command whose name is two words is not a typo of its first
        // word, it is that word plus the half the reader has not typed
        // yet -- and edit distance, which sees seven insertions, would
        // never suggest it. Ranked at zero so it wins outright.
        let d = if name.starts_with(typed) && name[typed.len()..].starts_with(' ') {
            0
        } else {
            distance(typed, name)
        };
        if d > budget && d != 0 {
            continue;
        }
        if best.is_none_or(|(bd, bn)| (d, name.len()) < (bd, bn.len())) {
            best = Some((d, name));
        }
    }
    best.map(|(_, name)| name)
}
