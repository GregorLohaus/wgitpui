//! Tiny per-line syntax highlighter and word-level diff. Deliberately heuristic: no grammars,
//! just enough lexing (comments, strings, numbers, keywords, types, calls) to make diffs readable.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
    Type,
    Func,
    Punct,
}

pub struct Lang {
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    /// Whether `'` starts a string (false for Rust, where it's usually a lifetime).
    single_quote_strings: bool,
}

const RUST: Lang = Lang {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for",
        "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
        "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "Some", "None", "Ok", "Err",
    ],
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    single_quote_strings: false,
};

const CLIKE: Lang = Lang {
    keywords: &[
        "abstract", "async", "await", "break", "case", "catch", "class", "const", "continue", "default", "defer", "delete",
        "do", "else", "enum", "export", "extends", "false", "final", "finally", "for", "func", "function", "go", "if",
        "implements", "import", "in", "instanceof", "interface", "let", "new", "nil", "null", "package", "private",
        "protected", "public", "return", "static", "struct", "super", "switch", "this", "throw", "true", "try", "type",
        "typeof", "var", "void", "while", "yield", "int", "char", "bool", "float", "double", "long", "unsigned", "auto",
        "include", "define", "namespace", "template", "typename", "undefined", "from", "of", "chan", "map", "range",
    ],
    line_comments: &["//"],
    block: Some(("/*", "*/")),
    single_quote_strings: true,
};

const PYTHON: Lang = Lang {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
        "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or",
        "pass", "raise", "return", "self", "True", "try", "while", "with", "yield",
    ],
    line_comments: &["#"],
    block: None,
    single_quote_strings: true,
};

const SHELL: Lang = Lang {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "function", "return", "in",
        "local", "export", "echo", "exit", "set", "true", "false",
    ],
    line_comments: &["#"],
    block: None,
    single_quote_strings: true,
};

const CONFIG: Lang = Lang {
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comments: &["#"],
    block: None,
    single_quote_strings: true,
};

const JSON: Lang = Lang { keywords: &["true", "false", "null"], line_comments: &[], block: None, single_quote_strings: false };

pub fn lang_for(path: &str) -> Option<&'static Lang> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => &RUST,
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" | "m" | "mm" | "java" | "kt" | "kts" | "cs" | "swift" | "go" | "js"
        | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "dart" | "scala" | "zig" | "wgsl" | "glsl" | "hlsl" | "css" | "scss" => &CLIKE,
        "py" | "pyi" => &PYTHON,
        "sh" | "bash" | "zsh" | "fish" => &SHELL,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "nix" => &CONFIG,
        "json" | "jsonc" => &JSON,
        _ => return None,
    })
}

/// Appends (start, end, token) byte spans covering `line` to `out`.
pub fn highlight(lang: Option<&Lang>, line: &str, out: &mut Vec<(usize, usize, Tok)>) {
    out.clear();
    let Some(lang) = lang else {
        out.push((0, line.len(), Tok::Plain));
        return;
    };
    let b = line.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut plain_start = 0;
    let flush = |out: &mut Vec<(usize, usize, Tok)>, s: usize, e: usize| {
        if e > s {
            out.push((s, e, Tok::Plain));
        }
    };
    while i < n {
        let rest = &line[i..];
        if let Some(_) = lang.line_comments.iter().find(|c| rest.starts_with(**c)) {
            flush(out, plain_start, i);
            out.push((i, n, Tok::Comment));
            return;
        }
        if let Some((open, close)) = lang.block {
            if rest.starts_with(open) {
                flush(out, plain_start, i);
                let end = rest[open.len()..].find(close).map_or(n, |p| i + open.len() + p + close.len());
                out.push((i, end, Tok::Comment));
                i = end;
                plain_start = i;
                continue;
            }
        }
        let c = b[i];
        if c == b'"' || c == b'`' || (c == b'\'' && (lang.single_quote_strings || closes_char_literal(b, i))) {
            flush(out, plain_start, i);
            let mut j = i + 1;
            while j < n && b[j] != c {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(n);
            out.push((i, end, Tok::Str));
            i = end;
            plain_start = i;
            continue;
        }
        if c.is_ascii_digit() && (i == 0 || !is_ident(b[i - 1])) {
            flush(out, plain_start, i);
            let mut j = i;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
                j += 1;
            }
            out.push((i, j, Tok::Number));
            i = j;
            plain_start = i;
            continue;
        }
        if is_ident_start(c) {
            let mut j = i;
            while j < n && is_ident(b[j]) {
                j += 1;
            }
            let word = &line[i..j];
            let next = line[j..].trim_start().as_bytes().first().copied();
            let tok = if lang.keywords.contains(&word) {
                Tok::Keyword
            } else if next == Some(b'(') || next == Some(b'!') {
                Tok::Func
            } else if c.is_ascii_uppercase() && word.len() > 1 {
                Tok::Type
            } else {
                Tok::Plain
            };
            if tok != Tok::Plain {
                flush(out, plain_start, i);
                out.push((i, j, tok));
                plain_start = j;
            }
            i = j;
            continue;
        }
        if c.is_ascii_punctuation() {
            flush(out, plain_start, i);
            out.push((i, i + 1, Tok::Punct));
            i += 1;
            plain_start = i;
            continue;
        }
        // Advance one UTF-8 scalar.
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    flush(out, plain_start, n);
}

fn closes_char_literal(b: &[u8], i: usize) -> bool {
    // 'a' or '\n' — but not a Rust lifetime like 'a or 'static.
    (b.get(i + 2) == Some(&b'\'')) || (b.get(i + 1) == Some(&b'\\') && b.get(i + 3) == Some(&b'\''))
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

// ---------------------------------------------------------------------------------------------
// Word diff
// ---------------------------------------------------------------------------------------------

fn tokens(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if is_ident(b[i]) {
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
        } else if b[i].is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
        out.push((start, i));
    }
    out
}

/// Byte ranges that differ between a removed line `a` and its paired added line `b`.
/// Returns empty ranges when the lines are too different for word highlighting to help.
pub fn word_diff(a: &str, b: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let ta = tokens(a);
    let tb = tokens(b);
    let (na, nb) = (ta.len(), tb.len());
    if na == 0 || nb == 0 || na * nb > 60_000 {
        return (vec![], vec![]);
    }
    let eq = |i: usize, j: usize| a[ta[i].0..ta[i].1] == b[tb[j].0..tb[j].1];
    // LCS table, filled from the end so we can walk forward.
    let w = nb + 1;
    let mut dp = vec![0u16; (na + 1) * (nb + 1)];
    for i in (0..na).rev() {
        for j in (0..nb).rev() {
            dp[i * w + j] = if eq(i, j) { dp[(i + 1) * w + j + 1] + 1 } else { dp[(i + 1) * w + j].max(dp[i * w + j + 1]) };
        }
    }
    let (mut keep_a, mut keep_b) = (vec![false; na], vec![false; nb]);
    let (mut i, mut j) = (0, 0);
    while i < na && j < nb {
        if eq(i, j) {
            keep_a[i] = true;
            keep_b[j] = true;
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let ranges = |toks: &[(usize, usize)], keep: &[bool], s: &str| -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (k, &(st, en)) in toks.iter().enumerate() {
            if keep[k] {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.1 == st || s[last.1..st].trim().is_empty() => last.1 = en,
                _ => out.push((st, en)),
            }
        }
        out
    };
    let ra = ranges(&ta, &keep_a, a);
    let rb = ranges(&tb, &keep_b, b);
    let changed = |r: &[(usize, usize)]| r.iter().map(|x| x.1 - x.0).sum::<usize>();
    let common = dp[0] as usize;
    // Mostly rewritten lines: whole-line coloring already says it all.
    if common * 3 < na.min(nb) || (changed(&ra) * 10 > a.len() * 7 && changed(&rb) * 10 > b.len() * 7) {
        return (vec![], vec![]);
    }
    (ra, rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust() {
        let mut out = Vec::new();
        let line = r#"let x = foo("hi", 42); // done"#;
        highlight(lang_for("a.rs"), line, &mut out);
        let kinds: Vec<(&str, Tok)> = out.iter().map(|&(s, e, t)| (&line[s..e], t)).filter(|x| x.1 != Tok::Plain && x.1 != Tok::Punct).collect();
        assert_eq!(kinds, [("let", Tok::Keyword), ("foo", Tok::Func), ("\"hi\"", Tok::Str), ("42", Tok::Number), ("// done", Tok::Comment)]);
        // Spans tile the whole line.
        assert_eq!(out.first().unwrap().0, 0);
        assert_eq!(out.last().unwrap().1, line.len());
        assert!(out.windows(2).all(|w| w[0].1 == w[1].0));
    }

    #[test]
    fn lifetimes_are_not_strings() {
        let mut out = Vec::new();
        highlight(lang_for("a.rs"), "fn f<'a>(x: &'a str) -> char { 'x' }", &mut out);
        let strs: Vec<_> = out.iter().filter(|s| s.2 == Tok::Str).collect();
        assert_eq!(strs.len(), 1);
    }

    #[test]
    fn word_diff_finds_changed_token() {
        let (a, b) = word_diff("let count = items.len();", "let count = items.len() + 1;");
        assert!(a.is_empty());
        assert_eq!(b.len(), 1);
        let (a, b) = word_diff("    foo(bar, baz)", "    foo(bar, qux)");
        assert_eq!(&"    foo(bar, baz)"[a[0].0..a[0].1], "baz");
        assert_eq!(&"    foo(bar, qux)"[b[0].0..b[0].1], "qux");
        let (a2, b2) = ("use std::collections::HashMap;", "use std::collections::{HashMap, VecDeque};");
        let (ra, rb) = word_diff(a2, b2);
        assert!(ra.is_empty());
        let changed: Vec<&str> = rb.iter().map(|&(s, e)| &b2[s..e]).collect();
        assert_eq!(changed, ["{", ", VecDeque}"]);
    }
}
