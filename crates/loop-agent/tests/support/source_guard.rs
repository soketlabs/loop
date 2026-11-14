//! Helpers for source guards: the production code of a Rust source file
//! (comments, literals and `#[cfg(test)]` items blanked) and the function
//! each line is in. Shared by `#[path]`; the same scan as
//! `tools_no_host_fs.rs`, which keeps its own copy.
#![allow(dead_code)]

/// `text` with comments and the insides of string and character literals
/// replaced by spaces (newlines stay, so line numbers hold).
pub fn blank_literals(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = b.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            let end = b[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(b.len(), |p| i + p);
            blank(&mut out, i, end);
            i = end;
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            let mut j = i;
            while j < b.len() {
                if b[j] == '/' && b.get(j + 1) == Some(&'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == '*' && b.get(j + 1) == Some(&'/') {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if c == 'r' || (c == 'b' && next == Some('r')) {
            // A raw string: r"..", r#".."#, br"..".
            let start = if c == 'b' { i + 1 } else { i };
            let mut j = start + 1;
            while b.get(j) == Some(&'#') {
                j += 1;
            }
            let prev_ident = i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_');
            if b.get(j) == Some(&'"') && !prev_ident {
                let hashes = j - start - 1;
                let close: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let mut k = j + 1;
                while k + close.len() <= b.len() && b[k..k + close.len()] != close[..] {
                    k += 1;
                }
                let end = (k + close.len()).min(b.len());
                blank(&mut out, j + 1, k.min(b.len()));
                i = end;
            } else {
                i += 1;
            }
        } else if c == '"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != '"' {
                j += if b[j] == '\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j.min(b.len()));
            i = j + 1;
        } else if c == '\'' {
            // A character literal, not a lifetime.
            let close = if next == Some('\\') {
                b[i + 2..]
                    .iter()
                    .position(|&c| c == '\'')
                    .map(|p| i + 2 + p)
            } else if b.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match close {
                Some(end) => {
                    blank(&mut out, i + 1, end);
                    i = end + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// Production code of a source file: literals blanked and every
/// `#[cfg(test)]` item (a module, function or `use`) removed.
pub fn production_code(text: &str) -> String {
    let blanked = blank_literals(text);
    let mut b: Vec<char> = blanked.chars().collect();
    let attrs: Vec<Vec<char>> = ["#[cfg(test)]", "#[cfg(all(test,"]
        .iter()
        .map(|a| a.chars().collect())
        .collect();
    let mut i = 0;
    while i < b.len() {
        let Some(attr) = attrs
            .iter()
            .find(|a| i + a.len() <= b.len() && b[i..i + a.len()] == a[..])
        else {
            i += 1;
            continue;
        };
        // The item runs from the attribute to the `;` or the closing brace.
        let mut j = i + attr.len();
        let mut depth = 0usize;
        let mut entered = false;
        while j < b.len() {
            match b[j] {
                '{' => {
                    depth += 1;
                    entered = true;
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    if entered && depth == 0 {
                        j += 1;
                        break;
                    }
                }
                ';' if !entered => {
                    j += 1;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        for c in &mut b[i..j] {
            if *c != '\n' {
                *c = ' ';
            }
        }
        i = j;
    }
    b.into_iter().collect()
}

/// The functions of `code` (production code from [`production_code`]), as
/// `(first line, last line, name)`, 1-based. A function without a body is
/// left out.
pub fn functions(code: &str) -> Vec<(usize, usize, String)> {
    let b: Vec<char> = code.chars().collect();
    let line_of = |at: usize| b[..at].iter().filter(|&&c| c == '\n').count() + 1;
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        let starts =
            b[i] == 'f' && b[i + 1] == 'n' && b[i + 2] == ' ' && (i == 0 || !ident(b[i - 1]));
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        while j < b.len() && b[j] == ' ' {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && ident(b[j]) {
            j += 1;
        }
        let name: String = b[name_start..j].iter().collect();
        // The body starts at the first `{` before any `;`.
        let mut k = j;
        while k < b.len() && b[k] != '{' && b[k] != ';' {
            k += 1;
        }
        if k >= b.len() || b[k] == ';' || name.is_empty() {
            i = j;
            continue;
        }
        let mut depth = 0usize;
        let mut end = k;
        while end < b.len() {
            match b[end] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        out.push((
            line_of(i),
            line_of(end.min(b.len().saturating_sub(1))),
            name,
        ));
        i = j;
    }
    out
}

/// The innermost function that holds `line`.
pub fn function_at(functions: &[(usize, usize, String)], line: usize) -> Option<&str> {
    functions
        .iter()
        .filter(|(first, last, _)| *first <= line && line <= *last)
        .max_by_key(|(first, _, _)| *first)
        .map(|(_, _, name)| name.as_str())
}
