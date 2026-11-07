//! Formulas between Excel syntax (`SUM(A1:B2,Sheet2!C3)`), which the model
//! reads and writes for every spreadsheet, and OpenFormula as stored in .ods
//! files (`of:=SUM([.A1:.B2];[$Sheet2.C3])`).

/// Excel formula (without `=`) as an .ods `table:formula` value.
pub(super) fn to_open_formula(excel: &str) -> String {
    let s: Vec<char> = excel.chars().collect();
    let mut out = String::from("of:=");
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == '"' {
            i = copy_string(&s, i, &mut out);
            continue;
        }
        if c == ',' {
            out.push(';');
            i += 1;
            continue;
        }
        let starts_token = i == 0 || !is_name_char(s[i - 1]);
        if starts_token && (c == '\'' || c == '$' || c.is_alphanumeric() || c == '_') {
            if let Some((end, reference)) = excel_reference(&s, i) {
                out.push_str(&reference.open_formula());
                i = end;
                continue;
            }
            let start = i;
            if c == '\'' {
                i += 1;
                while i < s.len() && s[i] != '\'' {
                    i += 1;
                }
                i = (i + 1).min(s.len());
            } else {
                while i < s.len() && is_name_char(s[i]) {
                    i += 1;
                }
            }
            out.extend(&s[start..i]);
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// An .ods `table:formula` value in Excel syntax, without the leading `=`.
pub(super) fn to_excel(open: &str) -> String {
    let mut body = open.trim();
    if let Some((prefix, rest)) = body.split_once(':') {
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphanumeric())
            && rest.starts_with('=')
        {
            body = rest;
        }
    }
    let s: Vec<char> = body.strip_prefix('=').unwrap_or(body).chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            '"' => i = copy_string(&s, i, &mut out),
            ';' => {
                out.push(',');
                i += 1;
            }
            '[' => {
                let mut j = i + 1;
                let mut quoted = false;
                while j < s.len() && (quoted || s[j] != ']') {
                    if s[j] == '\'' {
                        quoted = !quoted;
                    }
                    j += 1;
                }
                let inner: String = s[i + 1..j.min(s.len())].iter().collect();
                match open_reference(&inner) {
                    Some(reference) => out.push_str(&reference),
                    None => {
                        out.push('[');
                        out.push_str(&inner);
                        out.push(']');
                    }
                }
                i = (j + 1).min(s.len());
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Excel formula text as stored in .xlsx, without the prefixes Excel adds
/// to newer functions.
pub(super) fn from_xlsx(stored: &str) -> String {
    stored
        .trim_start_matches('=')
        .replace("_xlfn._xlws.", "")
        .replace("_xlfn.", "")
        .replace("_xlws.", "")
        .replace("_xlpm.", "")
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '$')
}

/// Copy the string literal starting at `s[i] == '"'` (with `""` escapes).
fn copy_string(s: &[char], mut i: usize, out: &mut String) -> usize {
    out.push('"');
    i += 1;
    while i < s.len() {
        out.push(s[i]);
        if s[i] == '"' {
            if s.get(i + 1) == Some(&'"') {
                out.push('"');
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

#[derive(Debug, PartialEq)]
struct Reference {
    sheet: Option<String>,
    start: String,
    end: Option<String>,
}

impl Reference {
    fn open_formula(&self) -> String {
        let sheet = match &self.sheet {
            Some(name) if is_plain_name(name) => format!("${name}"),
            Some(name) => format!("$'{}'", name.replace('\'', "''")),
            None => String::new(),
        };
        match &self.end {
            Some(end) => format!("[{sheet}.{}:.{end}]", self.start),
            None => format!("[{sheet}.{}]", self.start),
        }
    }
}

fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Part {
    Cell,
    Column,
    Row,
}

/// A cell, column or row reference part (`$A$1`, `A`, `$3`) at `s[i]`.
fn reference_part(s: &[char], mut i: usize) -> Option<(usize, String, Part)> {
    let mut text = String::new();
    if s.get(i) == Some(&'$') {
        text.push('$');
        i += 1;
    }
    let letters_at = i;
    while i < s.len() && s[i].is_ascii_alphabetic() {
        text.push(s[i].to_ascii_uppercase());
        i += 1;
    }
    let letters = i - letters_at;
    if letters > 3 {
        return None;
    }
    if letters > 0 && s.get(i) == Some(&'$') {
        text.push('$');
        i += 1;
    }
    let digits_at = i;
    while i < s.len() && s[i].is_ascii_digit() {
        text.push(s[i]);
        i += 1;
    }
    let digits = i - digits_at;
    let part = match (letters > 0, digits > 0) {
        (true, true) => Part::Cell,
        (true, false) if !text.ends_with('$') => Part::Column,
        (false, true) => Part::Row,
        _ => return None,
    };
    Some((i, text, part))
}

/// An Excel reference (`A1`, `$A$1:B2`, `Sheet2!A1`, `'My sheet'!A:A`) at `s[i]`.
fn excel_reference(s: &[char], i: usize) -> Option<(usize, Reference)> {
    let mut j = i;
    let mut sheet = None;
    if s[i] == '\'' {
        let mut name = String::new();
        j += 1;
        loop {
            match s.get(j) {
                Some('\'') if s.get(j + 1) == Some(&'\'') => {
                    name.push('\'');
                    j += 2;
                }
                Some('\'') => break,
                Some(c) => {
                    name.push(*c);
                    j += 1;
                }
                None => return None,
            }
        }
        if s.get(j + 1) != Some(&'!') {
            return None;
        }
        sheet = Some(name);
        j += 2;
    } else {
        let mut k = i;
        while k < s.len() && (s[k].is_alphanumeric() || matches!(s[k], '_' | '.')) {
            k += 1;
        }
        if k > i && s.get(k) == Some(&'!') {
            sheet = Some(s[i..k].iter().collect());
            j = k + 1;
        }
    }
    let (after, start, kind) = reference_part(s, j)?;
    let (end_at, end) = if s.get(after) == Some(&':') {
        let (end_at, end, end_kind) = reference_part(s, after + 1)?;
        if end_kind != kind {
            return None;
        }
        (end_at, Some(end))
    } else if kind == Part::Cell {
        (after, None)
    } else {
        return None;
    };
    if s.get(end_at)
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '(' | '.' | '!' | '$'))
    {
        return None;
    }
    Some((end_at, Reference { sheet, start, end }))
}

/// The inside of an OpenFormula reference (`.A1:.B2`, `$'My sheet'.A1`) in Excel syntax.
fn open_reference(inner: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut current = String::new();
    for c in inner.chars() {
        if c == '\'' {
            quoted = !quoted;
        }
        if c == ':' && !quoted {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    if parts.len() > 2 {
        return None;
    }
    let mut sheet = None;
    let mut cells = Vec::new();
    for part in &parts {
        let part = part.strip_prefix('$').unwrap_or(part);
        let (name, cell) = split_sheet(part)?;
        if sheet.is_none() && !name.is_empty() {
            sheet = Some(name);
        }
        cells.push(cell.to_owned());
    }
    let prefix = match sheet {
        Some(name) if is_plain_name(&name) => format!("{name}!"),
        Some(name) => format!("'{}'!", name.replace('\'', "''")),
        None => String::new(),
    };
    Some(format!("{prefix}{}", cells.join(":")))
}

/// `Sheet.A1` / `'My sheet'.A1` / `.A1` into the unquoted sheet name and cell.
fn split_sheet(part: &str) -> Option<(String, &str)> {
    if let Some(rest) = part.strip_prefix('\'') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if c == '\'' {
                if chars.peek().map(|(_, n)| *n) == Some('\'') {
                    name.push('\'');
                    chars.next();
                    continue;
                }
                let cell = rest[at + 1..].strip_prefix('.')?;
                return Some((name, cell));
            }
            name.push(c);
        }
        return None;
    }
    let dot = part.rfind('.')?;
    Some((part[..dot].to_owned(), &part[dot + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excel_to_open_formula() {
        assert_eq!(
            to_open_formula("SUM(A1:B2,$C$3)"),
            "of:=SUM([.A1:.B2];[.$C$3])"
        );
        assert_eq!(to_open_formula("Sheet2!A1*2"), "of:=[$Sheet2.A1]*2");
        assert_eq!(
            to_open_formula("SUM('My Sheet'!A:A)"),
            "of:=SUM([$'My Sheet'.A:.A])"
        );
        assert_eq!(
            to_open_formula("IF(B2>0,\"a,b\",LOG10(B2))"),
            "of:=IF([.B2]>0;\"a,b\";LOG10([.B2]))"
        );
        assert_eq!(
            to_open_formula("VLOOKUP(a2,Data!$A$2:$B$9,2,FALSE)"),
            "of:=VLOOKUP([.A2];[$Data.$A$2:.$B$9];2;FALSE)"
        );
    }

    #[test]
    fn open_formula_to_excel() {
        assert_eq!(to_excel("of:=SUM([.A1:.B2];[.$C$3])"), "SUM(A1:B2,$C$3)");
        assert_eq!(to_excel("of:=[$Sheet2.A1]*2"), "Sheet2!A1*2");
        assert_eq!(
            to_excel("of:=SUM([$'My Sheet'.A1:.A9])"),
            "SUM('My Sheet'!A1:A9)"
        );
        assert_eq!(to_excel("of:=IF([.B2]>0;\"a;b\";1)"), "IF(B2>0,\"a;b\",1)");
    }

    #[test]
    fn xlsx_prefixes_are_dropped() {
        assert_eq!(from_xlsx("_xlfn.STDEV.S(A1:A3)"), "STDEV.S(A1:A3)");
    }
}
