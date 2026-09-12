//! Syntax highlighting for the blob view, by file extension.
//!
//! **Why this is a lexer and not a parser.** Highlighting wants to know
//! which bytes are a comment, which are a string, and which are a
//! keyword. None of those questions needs a grammar, and every one of
//! them needs to be answered for a file that does not parse -- a
//! conflicted merge, a truncated paste, a language this node has never
//! heard of. A parser is the wrong shape: it fails on exactly the files
//! a reader most wants to look at.
//!
//! **One lexer, a table per language.** The differences between a
//! hundred languages at this level are four lists: what starts a line
//! comment, what brackets a block comment, what quotes a string, and
//! what words are reserved. So there is one scanner and a `Rules` per
//! family, which is what keeps the whole of this under a screen of
//! logic rather than a file per language.
//!
//! **No dependency.** `syntect` and friends carry grammar files, a
//! regex engine and an onig build; this crate hand-rolls base64 and
//! shells out for RS256 rather than take a parser, and a highlighter is
//! not where that bar moves.
//!
//! What it deliberately does not do: no semantic colour (a type is an
//! identifier that starts with a capital, and that is a guess), no
//! nested block comments, no interpolation inside strings, no here-docs.
//! Each of those is a real thing in some language and each would be
//! wrong somewhere else; the failure mode of all of them is a run of
//! text in the wrong colour, which is a cosmetic defect in a view whose
//! job is to be readable.

/// What a run of bytes is, as far as colour is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Plain,
    Comment,
    Str,
    Num,
    Kw,
    Lit,
    Type,
    Func,
    Macro,
}

impl Kind {
    /// The class this run is wrapped in, or `None` for the common case.
    ///
    /// Plain text gets no element at all. On a file that is mostly
    /// identifiers and punctuation that is most of it, and a `<span>`
    /// around every word would be the difference between a page and a
    /// download.
    fn class(self) -> Option<&'static str> {
        match self {
            Kind::Plain => None,
            Kind::Comment => Some("c"),
            Kind::Str => Some("s"),
            Kind::Num => Some("n"),
            Kind::Kw => Some("k"),
            Kind::Lit => Some("l"),
            Kind::Type => Some("t"),
            Kind::Func => Some("fn"),
            Kind::Macro => Some("m"),
        }
    }
}

/// Everything the scanner needs to know about one family of languages.
struct Rules {
    /// What runs to the end of the line.
    line: &'static [&'static str],
    /// What brackets a block, as `(open, close)`.
    block: &'static [(&'static str, &'static str)],
    /// What quotes a string.
    quotes: &'static [char],
    /// Whether a tripled quote opens a string that may contain
    /// newlines, which is Python's and nobody else's at this level.
    triple: bool,
    /// Whether a backslash escapes the next byte inside a string. False
    /// for the config formats, where a trailing backslash is a path.
    escapes: bool,
    /// Reserved words.
    keywords: &'static [&'static str],
    /// Words that are values rather than syntax.
    literals: &'static [&'static str],
    /// Whether `name!` is a macro call, which is Rust's spelling.
    bang_macros: bool,
}

const NONE: &[&str] = &[];
const NO_BLOCK: &[(&str, &str)] = &[];
const C_BLOCK: &[(&str, &str)] = &[("/*", "*/")];
const TRUTHS: &[&str] = &[
    "true", "false", "null", "nil", "none", "None", "True", "False",
];

const RUST: Rules = Rules {
    line: &["//"],
    block: C_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: true,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
        "union", "unsafe", "use", "where", "while", "yield",
    ],
    literals: &["true", "false", "None", "Some", "Ok", "Err"],
    bang_macros: true,
};

const PYTHON: Rules = Rules {
    line: &["#"],
    block: NO_BLOCK,
    quotes: &['"', '\''],
    triple: true,
    escapes: true,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
        "with", "yield",
    ],
    literals: &["True", "False", "None", "self", "cls"],
    bang_macros: false,
};

const JS: Rules = Rules {
    line: &["//"],
    block: C_BLOCK,
    quotes: &['"', '\'', '`'],
    triple: false,
    escapes: true,
    keywords: &[
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "finally",
        "for",
        "function",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "new",
        "of",
        "package",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "set",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "try",
        "type",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ],
    literals: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    bang_macros: false,
};

const GO: Rules = Rules {
    line: &["//"],
    block: C_BLOCK,
    quotes: &['"', '`', '\''],
    triple: false,
    escapes: true,
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ],
    literals: &["true", "false", "nil", "iota"],
    bang_macros: false,
};

const C_FAMILY: Rules = Rules {
    line: &["//"],
    block: C_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: true,
    keywords: &[
        "alignas",
        "alignof",
        "auto",
        "bool",
        "break",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "constexpr",
        "continue",
        "default",
        "delete",
        "do",
        "double",
        "else",
        "enum",
        "explicit",
        "extends",
        "extern",
        "final",
        "float",
        "for",
        "friend",
        "goto",
        "if",
        "implements",
        "import",
        "inline",
        "int",
        "interface",
        "long",
        "namespace",
        "new",
        "operator",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "register",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "try",
        "typedef",
        "typename",
        "union",
        "unsigned",
        "using",
        "virtual",
        "void",
        "volatile",
        "while",
    ],
    literals: &["true", "false", "nullptr", "NULL", "this"],
    bang_macros: false,
};

const SHELL: Rules = Rules {
    line: &["#"],
    block: NO_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: true,
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function",
        "if", "in", "local", "read", "readonly", "return", "set", "shift", "source", "then",
        "trap", "unset", "until", "while",
    ],
    literals: TRUTHS,
    bang_macros: false,
};

const SQL: Rules = Rules {
    line: &["--"],
    block: C_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: false,
    keywords: &[
        "alter", "and", "as", "by", "case", "create", "delete", "drop", "else", "end", "from",
        "group", "having", "index", "inner", "insert", "into", "join", "left", "limit", "not",
        "null", "on", "or", "order", "outer", "primary", "select", "set", "table", "then", "union",
        "update", "values", "where", "with",
    ],
    literals: TRUTHS,
    bang_macros: false,
};

const CONFIG: Rules = Rules {
    line: &["#"],
    block: NO_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: false,
    keywords: NONE,
    literals: TRUTHS,
    bang_macros: false,
};

const JSON: Rules = Rules {
    line: &[],
    block: NO_BLOCK,
    quotes: &['"'],
    triple: false,
    escapes: true,
    keywords: NONE,
    literals: &["true", "false", "null"],
    bang_macros: false,
};

const MARKUP: Rules = Rules {
    line: &[],
    block: &[("<!--", "-->")],
    quotes: &['"', '\''],
    triple: false,
    escapes: false,
    keywords: NONE,
    literals: NONE,
    bang_macros: false,
};

const CSS: Rules = Rules {
    line: &["//"],
    block: C_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: true,
    keywords: NONE,
    literals: NONE,
    bang_macros: false,
};

const LUA_LIKE: Rules = Rules {
    line: &["--"],
    block: NO_BLOCK,
    quotes: &['"', '\''],
    triple: false,
    escapes: true,
    keywords: &[
        "and", "break", "case", "class", "data", "do", "else", "elseif", "end", "for", "function",
        "if", "import", "in", "instance", "let", "local", "module", "not", "or", "repeat",
        "return", "then", "type", "until", "where", "while",
    ],
    literals: &["true", "false", "nil"],
    bang_macros: false,
};

const RUBY_LIKE: Rules = Rules {
    line: &["#"],
    block: NO_BLOCK,
    quotes: &['"', '\'', '`'],
    triple: false,
    escapes: true,
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defined", "do", "else", "elsif",
        "end", "ensure", "for", "if", "in", "module", "next", "not", "or", "redo", "rescue",
        "retry", "return", "self", "super", "then", "undef", "unless", "until", "when", "while",
        "yield", "use", "require", "function", "echo", "print", "my", "our", "sub", "package",
    ],
    literals: &["true", "false", "nil", "null", "self"],
    bang_macros: false,
};

/// The rules for a path, or `None` for a file this does not colour.
///
/// Extension first and then the whole filename, because the files that
/// carry no extension are exactly the ones a repository root is full of
/// -- `Makefile`, `Dockerfile`, `.gitignore` -- and leaving them plain
/// is the most visible gap this could have.
fn rules_for(path: &str) -> Option<&'static Rules> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let by_ext = match ext {
        "rs" => Some(&RUST),
        "py" | "pyi" | "pyw" => Some(&PYTHON),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" => Some(&JS),
        "go" => Some(&GO),
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" | "hh" | "m" | "mm" | "java" | "kt"
        | "kts" | "swift" | "scala" | "cs" | "dart" | "zig" | "v" | "d" | "groovy" | "gradle"
        | "proto" | "glsl" | "wgsl" | "rs_in" => Some(&C_FAMILY),
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "ps1" | "bat" | "cmd" => Some(&SHELL),
        "sql" => Some(&SQL),
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "properties" | "tf" | "hcl"
        | "nix" | "gitignore" | "gitattributes" | "editorconfig" | "dockerignore" => Some(&CONFIG),
        "json" | "jsonc" | "json5" | "lock" | "webmanifest" => Some(&JSON),
        "html" | "htm" | "xml" | "svg" | "xhtml" | "vue" | "svelte" | "rss" | "atom" | "plist"
        | "xsl" => Some(&MARKUP),
        "css" | "scss" | "sass" | "less" | "styl" => Some(&CSS),
        "lua" | "hs" | "elm" | "sql_lua" | "moon" | "applescript" => Some(&LUA_LIKE),
        "rb" | "rake" | "gemspec" | "pl" | "pm" | "php" | "r" | "jl" | "ex" | "exs" | "erl"
        | "nim" | "cr" | "tcl" | "vim" | "coffee" => Some(&RUBY_LIKE),
        _ => None,
    };
    if by_ext.is_some() {
        return by_ext;
    }
    // No extension, or one nobody claims. The name itself is the last
    // thing worth asking, and these are the ones that actually turn up.
    match lower.as_str() {
        "makefile" | "gnumakefile" | "justfile" | "cmakelists.txt" | "dockerfile" | "procfile"
        | "vagrantfile" | "brewfile" | "rakefile" | "gemfile" => Some(&CONFIG),
        "gate" | "choirctl" | "configure" | "bashrc" | "zshrc" | "profile" => Some(&SHELL),
        _ => None,
    }
}

/// Whether a byte can be inside an identifier.
fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The runs of `text`, in order, covering every byte exactly once.
fn scan(text: &str, rules: &Rules) -> Vec<(usize, usize, Kind)> {
    let bytes = text.as_bytes();
    let mut runs: Vec<(usize, usize, Kind)> = Vec::new();
    let mut plain_from = 0usize;
    let mut at = 0usize;

    // A closure would have to borrow `runs` mutably while `plain_from`
    // is read, so this is a macro over the two locals instead.
    macro_rules! flush {
        ($to:expr) => {
            if $to > plain_from {
                runs.push((plain_from, $to, Kind::Plain));
            }
        };
    }

    while at < text.len() {
        let rest = &text[at..];
        let ch = match rest.chars().next() {
            Some(c) => c,
            None => break,
        };

        // A line comment.
        if rules.line.iter().any(|o| rest.starts_with(*o)) {
            let end = rest.find('\n').map_or(text.len(), |n| at + n);
            flush!(at);
            runs.push((at, end, Kind::Comment));
            at = end;
            plain_from = at;
            continue;
        }

        // A block comment, closed or running to the end of the file.
        if let Some((open, close)) = rules.block.iter().find(|(o, _)| rest.starts_with(*o)) {
            let from = at + open.len();
            let end = text[from..]
                .find(close)
                .map_or(text.len(), |n| from + n + close.len());
            flush!(at);
            runs.push((at, end, Kind::Comment));
            at = end;
            plain_from = at;
            continue;
        }

        // A tripled quote, which may hold newlines.
        if rules.triple && (rest.starts_with("\"\"\"") || rest.starts_with("'''")) {
            let fence = &rest[..3];
            let from = at + 3;
            let end = text[from..]
                .find(fence)
                .map_or(text.len(), |n| from + n + 3);
            flush!(at);
            runs.push((at, end, Kind::Str));
            at = end;
            plain_from = at;
            continue;
        }

        // A string. It stops at its own quote, at a newline, or at the
        // end of the file -- the newline because an unterminated quote
        // is a typo, and colouring the rest of the document as a string
        // is how one typo makes a file unreadable.
        if rules.quotes.contains(&ch) {
            let opened = at + ch.len_utf8();
            let mut end = text.len();
            let mut escaped = false;
            for (offset, c) in text[opened..].char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if rules.escapes && c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '\n' {
                    end = opened + offset;
                    break;
                }
                if c == ch {
                    end = opened + offset + c.len_utf8();
                    break;
                }
            }
            flush!(at);
            runs.push((at, end, Kind::Str));
            at = end;
            plain_from = at;
            continue;
        }

        // A number, but not the tail of an identifier: `utf8` is a word.
        let after_word = at > 0 && text[..at].chars().next_back().is_some_and(word);
        if ch.is_ascii_digit() && !after_word {
            let mut end = at;
            for (offset, c) in rest.char_indices() {
                if c.is_alphanumeric() || c == '.' || c == '_' {
                    end = at + offset + c.len_utf8();
                } else {
                    break;
                }
            }
            flush!(at);
            runs.push((at, end, Kind::Num));
            at = end;
            plain_from = at;
            continue;
        }

        // A word.
        if ch.is_alphabetic() || ch == '_' {
            let mut end = at;
            for (offset, c) in rest.char_indices() {
                if word(c) {
                    end = at + offset + c.len_utf8();
                } else {
                    break;
                }
            }
            let ident = &text[at..end];
            let kind = if rules.keywords.contains(&ident) {
                Kind::Kw
            } else if rules.literals.contains(&ident) {
                Kind::Lit
            } else if rules.bang_macros && bytes.get(end) == Some(&b'!') {
                Kind::Macro
            } else if bytes.get(end) == Some(&b'(') {
                Kind::Func
            } else if ident
                .chars()
                .next()
                .is_some_and(|c| c.is_uppercase() && ident.chars().any(|c| c.is_lowercase()))
            {
                Kind::Type
            } else {
                Kind::Plain
            };
            if kind != Kind::Plain {
                flush!(at);
                runs.push((at, end, kind));
                plain_from = end;
            }
            at = end;
            continue;
        }

        at += ch.len_utf8();
    }
    flush!(text.len());
    runs
}

/// Writes `text` into `out` as highlighted lines, each behind its
/// number, or returns `false` if this path is not one it colours.
///
/// The line is the unit the blob view is built from and a run is not:
/// a block comment crosses lines and a string may. So the runs are
/// computed over the whole file and then clipped to each line, which
/// re-opens the span on every line it covers -- the same thing every
/// editor does, and the reason a `<pre>` can carry line numbers at all.
pub(crate) fn highlight(out: &mut String, path: &str, text: &str) -> bool {
    let Some(rules) = rules_for(path) else {
        return false;
    };
    if text.is_empty() {
        return true;
    }
    let runs = scan(text, rules);
    let mut run = 0usize;
    let mut start = 0usize;
    for (n, line) in text.split('\n').enumerate() {
        let end = start + line.len();
        out.push_str("<span class=\"ln\">");
        out.push_str(&(n + 1).to_string());
        out.push_str("</span>");
        while run < runs.len() && runs[run].1 <= start {
            run += 1;
        }
        let mut cursor = run;
        while cursor < runs.len() && runs[cursor].0 < end {
            let (from, to, kind) = runs[cursor];
            let piece = &text[from.max(start)..to.min(end)];
            match kind.class() {
                Some(class) => {
                    out.push_str("<span class=\"");
                    out.push_str(class);
                    out.push_str("\">");
                    out.push_str(&crate::ui::esc(piece));
                    out.push_str("</span>");
                }
                None => out.push_str(&crate::ui::esc(piece)),
            }
            cursor += 1;
        }
        out.push('\n');
        start = end + 1;
    }
    // `split` yields one empty piece past a trailing newline, and that
    // piece is a line number for a line nobody wrote. It is dropped the
    // same way the plain renderer drops it, by `lines()`; here the count
    // is already emitted, so the trailing blank is trimmed instead.
    if text.ends_with('\n') {
        let tail = out.rfind("<span class=\"ln\">").unwrap_or(0);
        out.truncate(tail);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte of the input comes out exactly once, in order.
    ///
    /// This is the property that makes the rest safe to get wrong: a
    /// scanner that drops a run silently deletes source from the page,
    /// and a scanner that overlaps duplicates it. Both render as a file
    /// that looks almost right, which is the worst way for a code view
    /// to fail.
    #[test]
    fn the_runs_cover_every_byte_exactly_once() {
        let samples = [
            ("a.rs", "fn main() { let x = \"hi\"; /* c */ } // end\n"),
            ("a.py", "def f():\n    '''doc\nmore'''\n    return 1  # n\n"),
            ("a.js", "const a = `x${1}`; // c\n/* unclosed\n"),
            ("a.toml", "[x]\nk = \"v\" # note\n"),
            ("a.json", "{\"a\": [1, true, null]}\n"),
            ("a.rs", "let s = \"unterminated\nlet t = 2;\n"),
            ("a.rs", "// 日本語 comment\nlet emoji = \"🎵\";\n"),
            ("a.c", ""),
        ];
        for (path, text) in samples {
            let rules = rules_for(path).expect("a sample names a language");
            let runs = scan(text, rules);
            let mut at = 0;
            for (from, to, _) in &runs {
                assert_eq!(*from, at, "a gap or an overlap in {path}: {runs:?}");
                assert!(to > from || from == to, "an empty run in {path}");
                at = *to;
            }
            assert_eq!(at, text.len(), "the scan stopped short in {path}");
            let joined: String = runs.iter().map(|(f, t, _)| &text[*f..*t]).collect();
            assert_eq!(joined, text, "the runs do not rebuild {path}");
        }
    }

    /// The classes land on the things they name.
    #[test]
    fn a_rust_line_is_coloured_the_way_it_reads() {
        let mut out = String::new();
        assert!(highlight(
            &mut out,
            "src/lib.rs",
            "pub fn go() { println!(\"hi\"); }\n"
        ));
        assert!(out.contains("<span class=\"k\">pub</span>"), "{out}");
        assert!(out.contains("<span class=\"m\">println</span>"), "{out}");
        assert!(
            out.contains("<span class=\"s\">&quot;hi&quot;</span>"),
            "{out}"
        );
        assert!(out.contains("<span class=\"fn\">go</span>"), "{out}");
    }

    /// Python is not coloured as Rust, which is the whole point of
    /// choosing by extension.
    #[test]
    fn python_takes_its_own_rules() {
        let mut out = String::new();
        assert!(highlight(
            &mut out,
            "tool.py",
            "def f(x):\n    # note\n    return None\n"
        ));
        assert!(out.contains("<span class=\"k\">def</span>"), "{out}");
        assert!(out.contains("<span class=\"c\"># note</span>"), "{out}");
        assert!(out.contains("<span class=\"l\">None</span>"), "{out}");
        // `#` is a comment here and is not one in Rust.
        let mut rust = String::new();
        highlight(&mut rust, "a.rs", "# not a comment\n");
        assert!(!rust.contains("<span class=\"c\">"), "{rust}");
    }

    /// Nothing reaches the page unescaped, whatever the language.
    ///
    /// A blob is attacker-supplied by definition on a node serving
    /// somebody else's push, and this is the one code path that writes
    /// file contents into markup with spans interleaved -- the place a
    /// missed escape would hide.
    #[test]
    fn no_language_lets_markup_through() {
        for path in [
            "a.rs", "a.py", "a.js", "a.html", "a.json", "a.sh", "Makefile",
        ] {
            let mut out = String::new();
            assert!(highlight(
                &mut out,
                path,
                "<script>alert(1)</script>\n\"<img src=x onerror=alert(1)>\"\n"
            ));
            // The precise property: after the spans this module writes
            // are removed, nothing angled is left. Probing for `<script`
            // or for `onerror=` is weaker and, in the second case,
            // wrong -- `onerror=alert(1)` is harmless *text* once its
            // brackets are entities, and a test that forbids it is
            // forbidding a string a file may legitimately contain.
            let mut bare = out.clone();
            while let Some(at) = bare.find("<span class=\"") {
                let end = bare[at..].find('>').expect("a span tag closes") + at + 1;
                bare.replace_range(at..end, "");
            }
            bare = bare.replace("</span>", "");
            assert!(
                !bare.contains('<') && !bare.contains('>'),
                "{path} let markup through: {bare}"
            );
            assert!(
                out.contains("&lt;script&gt;"),
                "{path} lost the text: {out}"
            );
        }
    }

    /// A file this has no rules for says so rather than rendering
    /// plain-but-wrapped, because the caller has a perfectly good
    /// renderer for that case already.
    #[test]
    fn an_unknown_extension_is_declined() {
        let mut out = String::new();
        assert!(!highlight(&mut out, "notes.xyz", "anything\n"));
        assert!(out.is_empty(), "a declined file still wrote: {out}");
        assert!(rules_for("LICENSE-MIT").is_none());
        assert!(rules_for("Makefile").is_some());
        assert!(rules_for("gate").is_some());
    }

    /// The line count is the file's, not one more.
    #[test]
    fn a_trailing_newline_does_not_invent_a_line() {
        let mut with = String::new();
        highlight(&mut with, "a.rs", "one\ntwo\n");
        let mut without = String::new();
        highlight(&mut without, "a.rs", "one\ntwo");
        assert_eq!(with.matches("class=\"ln\"").count(), 2, "{with}");
        assert_eq!(without.matches("class=\"ln\"").count(), 2, "{without}");
    }
}
