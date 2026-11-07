//! The spreadsheet text format shared by `read` and `write` (see
//! `WRITE_GUIDE`), .xlsx/.ods reading through calamine, and .xlsx writing.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::LazyLock;

use calamine::{Data, Reader as _};
use regex::Regex;
use rust_xlsxwriter::{ExcelDateTime, Format, Formula, Workbook};

use super::formula;
use super::markdown::table_cell;

pub(super) struct Sheet {
    pub name: String,
    pub cells: Vec<Cell>,
}

pub(super) struct Cell {
    /// Zero-based.
    pub row: u32,
    /// Zero-based.
    pub col: u16,
    pub value: Value,
    pub bold: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    Text(String),
    Number(f64),
    /// A fraction shown as a percentage: `12.5%` is 0.125.
    Percent(f64),
    Bool(bool),
    Date(Date),
    /// Excel syntax, without the leading `=`.
    Formula(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub time: Option<(u8, u8, u8)>,
}

impl Date {
    /// ISO 8601, as .ods stores it.
    pub(super) fn iso(&self) -> String {
        let date = format!("{:04}-{:02}-{:02}", self.year, self.month, self.day);
        match self.time {
            Some((h, m, s)) => format!("{date}T{h:02}:{m:02}:{s:02}"),
            None => date,
        }
    }
}

const MAX_ROWS: u32 = 1_048_576;
const MAX_COLS: u16 = 16_384;

static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$").unwrap());
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4})-(\d{2})-(\d{2})(?:[ T](\d{2}):(\d{2})(?::(\d{2}))?)?$").unwrap()
});

/// The sheets described by `text` in the spreadsheet format.
pub(super) fn parse(text: &str) -> Result<Vec<Sheet>, String> {
    let mut sheets: Vec<Sheet> = Vec::new();
    let mut columns: Option<Vec<u16>> = None;
    let mut seen: HashSet<(u32, u16)> = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let line_no = n + 1;
        let trimmed = line.trim();
        if let Some(name) = sheet_heading(trimmed) {
            if name.is_empty() {
                return Err(format!(
                    "line {line_no}: the sheet needs a name, like `## Sheet: Budget`"
                ));
            }
            if sheets.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
                return Err(format!(
                    "line {line_no}: there are two sheets named `{name}`"
                ));
            }
            sheets.push(Sheet {
                name: name.to_owned(),
                cells: Vec::new(),
            });
            columns = None;
            seen.clear();
            continue;
        }
        if !trimmed.starts_with('|') {
            // Text between tables ends the table; the next one needs its own header.
            columns = None;
            continue;
        }
        let cells = split_row(trimmed);
        if cells.iter().all(|c| is_separator(c)) {
            continue;
        }
        if sheets.is_empty() {
            sheets.push(Sheet {
                name: "Sheet1".into(),
                cells: Vec::new(),
            });
        }
        let sheet = sheets.last_mut().unwrap();
        let Some(cols) = &columns else {
            columns = Some(header(&cells).map_err(|e| format!("line {line_no}: {e}"))?);
            continue;
        };
        let row: u32 = cells[0]
            .trim()
            .parse()
            .ok()
            .filter(|r| (1..=MAX_ROWS).contains(r))
            .ok_or_else(|| {
                format!("line {line_no}: each row must start with its row number, like `| 2 | ... |` (got `{}`)", cells[0].trim())
            })?;
        if cells.len() - 1 > cols.len() {
            return Err(format!(
                "line {line_no}: row {row} has {} cells but the header names {} columns",
                cells.len() - 1,
                cols.len()
            ));
        }
        for (raw, col) in cells[1..].iter().zip(cols) {
            let Some((value, bold)) = parse_cell(raw) else {
                continue;
            };
            if !seen.insert((row, *col)) {
                return Err(format!(
                    "line {line_no}: cell {} is given twice in sheet `{}`",
                    address(row - 1, *col),
                    sheet.name
                ));
            }
            sheet.cells.push(Cell {
                row: row - 1,
                col: *col,
                value,
                bold,
            });
        }
    }
    if sheets.is_empty() {
        return Err("no sheet found: write `## Sheet: <name>` followed by a table whose header row lists column letters (see the write tool's spreadsheet format)".into());
    }
    Ok(sheets)
}

fn sheet_heading(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches('#');
    if rest.len() == line.len() {
        return None;
    }
    let rest = rest.trim_start();
    rest.strip_prefix("Sheet:")
        .or_else(|| rest.strip_prefix("sheet:"))
        .map(str::trim)
}

/// The cells of a `| a | b |` row, with `\|` unescaped.
fn split_row(line: &str) -> Vec<String> {
    let inner = line.strip_prefix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        cells.push(current);
    }
    cells
}

fn is_separator(cell: &str) -> bool {
    let c = cell.trim().trim_matches(':');
    !c.is_empty() && c.chars().all(|ch| ch == '-')
}

fn header(cells: &[String]) -> Result<Vec<u16>, String> {
    let corner = cells.first().map(|c| c.trim()).unwrap_or("");
    if !matches!(corner, "" | "#") || cells.len() < 2 {
        return Err("the first row of a sheet table must list column letters after an empty cell, like `| | A | B | C |`".into());
    }
    let mut cols = Vec::new();
    for cell in &cells[1..] {
        let col = column_index(cell.trim()).ok_or_else(|| {
            format!(
                "`{}` in the header row is not a column letter (A, B, ..., Z, AA, ...)",
                cell.trim()
            )
        })?;
        if cols.contains(&col) {
            return Err(format!(
                "column {} appears twice in the header row",
                cell.trim()
            ));
        }
        cols.push(col);
    }
    Ok(cols)
}

/// Zero-based index of column letters (`A` is 0, `AA` is 26).
fn column_index(letters: &str) -> Option<u16> {
    if letters.is_empty() || letters.len() > 3 || !letters.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    let mut n: u32 = 0;
    for c in letters.chars() {
        n = n * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    let index = n - 1;
    (index < MAX_COLS as u32).then_some(index as u16)
}

pub(super) fn column_letters(col: u16) -> String {
    let mut n = col as u32 + 1;
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push((b'A' + rem as u8) as char);
        n = (n - 1) / 26;
    }
    out.iter().rev().collect()
}

pub(super) fn address(row: u32, col: u16) -> String {
    format!("{}{}", column_letters(col), row + 1)
}

/// The value and boldness of a cell's text; `None` for an empty cell.
fn parse_cell(raw: &str) -> Option<(Value, bool)> {
    let text = raw
        .trim()
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let (text, bold) = match text.strip_prefix("**").and_then(|t| t.strip_suffix("**")) {
        Some(inner) if !inner.is_empty() => (inner.trim().to_owned(), true),
        _ => (text, false),
    };
    if text.is_empty() {
        return None;
    }
    Some((classify(&text), bold))
}

fn classify(text: &str) -> Value {
    if let Some(literal) = text.strip_prefix('\'') {
        return Value::Text(literal.to_owned());
    }
    if let Some(formula) = text.strip_prefix('=') {
        if !formula.trim().is_empty() {
            return Value::Formula(formula.trim().to_owned());
        }
    }
    match text {
        "TRUE" | "true" | "True" => return Value::Bool(true),
        "FALSE" | "false" | "False" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = number(text) {
        return Value::Number(n);
    }
    if let Some(n) = text.strip_suffix('%').and_then(number) {
        return Value::Percent(n / 100.0);
    }
    if let Some(date) = date(text) {
        return Value::Date(date);
    }
    Value::Text(text.to_owned())
}

/// A plain number; leading zeros (`007`, zip codes) stay text.
fn number(text: &str) -> Option<f64> {
    if !NUMBER.is_match(text) {
        return None;
    }
    let digits = text.trim_start_matches(['-', '+']);
    if digits.len() > 1 && digits.starts_with('0') && !digits[1..].starts_with(['.', 'e', 'E']) {
        return None;
    }
    text.parse().ok().filter(|n: &f64| n.is_finite())
}

fn date(text: &str) -> Option<Date> {
    let caps = DATE.captures(text)?;
    let num = |i: usize| caps.get(i).map(|m| m.as_str().parse::<u32>().unwrap_or(0));
    let (year, month, day) = (num(1)?, num(2)?, num(3)?);
    chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?;
    if !(1900..=9999).contains(&year) {
        return None;
    }
    let time = match (num(4), num(5)) {
        (Some(h), Some(m)) => {
            let s = num(6).unwrap_or(0);
            if h > 23 || m > 59 || s > 59 {
                return None;
            }
            Some((h as u8, m as u8, s as u8))
        }
        _ => None,
    };
    Some(Date {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        time,
    })
}

/// The workbook in `bytes` (.xlsx or .ods) in the spreadsheet format.
pub(super) fn read(bytes: &[u8], ods: bool) -> Result<String, String> {
    let mut book = calamine::open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .map_err(|e| format!("could not open the spreadsheet: {e}"))?;
    let mut out = String::new();
    for name in book.sheet_names() {
        let values = book
            .worksheet_range(&name)
            .map_err(|e| format!("sheet `{name}`: {e}"))?;
        let formulas = book.worksheet_formula(&name).ok();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        render_sheet(&mut out, &name, &values, formulas.as_ref(), ods);
    }
    Ok(out)
}

fn render_sheet(
    out: &mut String,
    name: &str,
    values: &calamine::Range<Data>,
    formulas: Option<&calamine::Range<String>>,
    ods: bool,
) {
    // (row, col) -> (cell text, value saved for a formula)
    let mut cells: BTreeMap<(u32, u32), (String, Option<String>)> = BTreeMap::new();
    let (row0, col0) = values.start().unwrap_or((0, 0));
    for (r, c, value) in values.cells() {
        if let Some(text) = value_text(value) {
            cells.insert((row0 + r as u32, col0 + c as u32), (text, None));
        }
    }
    if let Some(formulas) = formulas {
        let (frow0, fcol0) = formulas.start().unwrap_or((0, 0));
        for (r, c, stored) in formulas.cells() {
            if stored.trim().is_empty() {
                continue;
            }
            let at = (frow0 + r as u32, fcol0 + c as u32);
            let excel = if ods {
                formula::to_excel(stored)
            } else {
                formula::from_xlsx(stored)
            };
            let saved = cells
                .remove(&at)
                .map(|(text, _)| text.trim_start_matches('\'').to_owned());
            cells.insert(at, (format!("={excel}"), saved));
        }
    }
    let _ = write!(out, "## Sheet: {name}\n\n");
    if cells.is_empty() {
        out.push_str("(empty)");
        return;
    }
    let first_col = cells.keys().map(|(_, c)| *c).min().unwrap_or(0);
    let last_col = cells.keys().map(|(_, c)| *c).max().unwrap_or(0);
    out.push_str("| |");
    for col in first_col..=last_col {
        let _ = write!(out, " {} |", column_letters(col as u16));
    }
    out.push_str("\n|---|");
    for _ in first_col..=last_col {
        out.push_str("---|");
    }
    let rows: BTreeSet<u32> = cells.keys().map(|(r, _)| *r).collect();
    for row in rows {
        let _ = write!(out, "\n| {} |", row + 1);
        for col in first_col..=last_col {
            match cells.get(&(row, col)) {
                Some((text, _)) => {
                    let _ = write!(out, " {} |", table_cell(text));
                }
                None => out.push_str(" |"),
            }
        }
    }
    let saved: Vec<String> = cells
        .iter()
        .filter_map(|((row, col), (_, saved))| {
            let saved = saved.as_ref()?;
            Some(format!(
                "- {}: {}",
                address(*row, *col as u16),
                saved.replace('\n', " ")
            ))
        })
        .collect();
    if !saved.is_empty() {
        out.push_str(
            "\n\nFormula values saved in the file (recalculated when opened; ignored by write):\n",
        );
        out.push_str(&saved.join("\n"));
    }
}

/// A stored value as cell text, quoted with `'` when it would otherwise
/// read back as another type.
fn value_text(value: &Data) -> Option<String> {
    let text = match value {
        Data::Empty => return None,
        Data::Int(n) => n.to_string(),
        Data::Float(f) => number_text(*f),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Data::String(s) => {
            if s.is_empty() {
                return None;
            }
            let reads_back = matches!(parse_cell(s), Some((Value::Text(t), false)) if t == *s);
            if reads_back {
                s.clone()
            } else {
                format!("'{s}")
            }
        }
        Data::DateTime(dt) => {
            if dt.is_duration() {
                number_text(dt.as_f64())
            } else {
                match dt.as_datetime() {
                    Some(dt) if dt.time() == chrono::NaiveTime::MIN => {
                        dt.format("%Y-%m-%d").to_string()
                    }
                    Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                    None => number_text(dt.as_f64()),
                }
            }
        }
        Data::DateTimeIso(s) => s.replacen('T', " ", 1),
        Data::DurationIso(s) => format!("'{s}"),
        Data::Error(e) => format!("'{e}"),
    };
    Some(text)
}

fn number_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// An .xlsx workbook of `sheets`; Excel recalculates the formulas on open.
pub(super) fn write_xlsx(sheets: &[Sheet]) -> Result<Vec<u8>, String> {
    let mut book = Workbook::new();
    for sheet in sheets {
        let ws = book.add_worksheet();
        ws.set_name(&sheet.name)
            .map_err(|e| format!("sheet name `{}`: {e}", sheet.name))?;
        // No made-up cached results (the default is 0) for readers that
        // don't recalculate.
        ws.set_formula_result_default("");
        for cell in &sheet.cells {
            let (row, col) = (cell.row, cell.col);
            let mut format = Format::new();
            if cell.bold {
                format = format.set_bold();
            }
            let written = match &cell.value {
                Value::Text(text) => {
                    if text.contains('\n') {
                        format = format.set_text_wrap();
                    }
                    ws.write_string_with_format(row, col, text, &format)
                }
                Value::Number(n) => ws.write_number_with_format(row, col, *n, &format),
                Value::Percent(n) => {
                    let pattern = if (n * 100.0).fract() == 0.0 {
                        "0%"
                    } else {
                        "0.00%"
                    };
                    ws.write_number_with_format(row, col, *n, &format.set_num_format(pattern))
                }
                Value::Bool(b) => ws.write_boolean_with_format(row, col, *b, &format),
                Value::Date(date) => {
                    let pattern = if date.time.is_some() {
                        "yyyy-mm-dd hh:mm:ss"
                    } else {
                        "yyyy-mm-dd"
                    };
                    let datetime = excel_datetime(date).map_err(|e| {
                        format!("sheet `{}` cell {}: {e}", sheet.name, address(row, col))
                    })?;
                    ws.write_datetime_with_format(
                        row,
                        col,
                        &datetime,
                        &format.set_num_format(pattern),
                    )
                }
                Value::Formula(f) => {
                    ws.write_formula_with_format(row, col, Formula::new(f), &format)
                }
            };
            written
                .map_err(|e| format!("sheet `{}` cell {}: {e}", sheet.name, address(row, col)))?;
        }
        ws.autofit();
    }
    book.save_to_buffer().map_err(|e| e.to_string())
}

fn excel_datetime(date: &Date) -> Result<ExcelDateTime, String> {
    let day =
        ExcelDateTime::from_ymd(date.year, date.month, date.day).map_err(|e| e.to_string())?;
    match date.time {
        Some((h, m, s)) => day.and_hms(h as u16, m, s).map_err(|e| e.to_string()),
        None => Ok(day),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: &str = "## Sheet: Budget\n\n| | A | B | C |\n|---|---|---|---|\n| 1 | **Item** | **Cost** | **Share** |\n| 2 | Rent | 1200 | =B2/B4 |\n| 3 | Food | 450.5 | =B3/B4 |\n| 4 | Total | =SUM(B2:B3) | 100% |\n\n## Sheet: Notes\n\n| | A | C |\n|---|---|---|\n| 1 | '00123 | 2024-01-31 |\n| 3 | a \\| b | TRUE |\n";

    #[test]
    fn parse_types_cells() {
        let sheets = parse(BUDGET).unwrap();
        assert_eq!(sheets.len(), 2);
        let budget = &sheets[0];
        let at = |r: u32, c: u16| {
            budget
                .cells
                .iter()
                .find(|x| x.row == r && x.col == c)
                .unwrap()
        };
        assert!(at(0, 0).bold);
        assert_eq!(at(1, 1).value, Value::Number(1200.0));
        assert_eq!(at(1, 2).value, Value::Formula("B2/B4".into()));
        assert_eq!(at(3, 2).value, Value::Percent(1.0));
        let notes = &sheets[1];
        assert_eq!(notes.cells[0].value, Value::Text("00123".into()));
        assert_eq!(notes.cells[1].col, 2);
        assert!(matches!(
            notes.cells[1].value,
            Value::Date(Date {
                year: 2024,
                month: 1,
                day: 31,
                time: None
            })
        ));
        assert_eq!(notes.cells[2].value, Value::Text("a | b".into()));
        assert_eq!(notes.cells[3].value, Value::Bool(true));
    }

    #[test]
    fn parse_explains_mistakes() {
        let err = parse("## Sheet: S\n| A | B |\n").err().unwrap();
        assert!(err.contains("column letters"), "{err}");
        let err = parse("## Sheet: S\n| | A |\n|---|---|\n| x | 1 |\n")
            .err()
            .unwrap();
        assert!(err.contains("row number"), "{err}");
        assert!(parse("just prose").is_err());
    }

    #[test]
    fn xlsx_round_trips_with_formulas() {
        let bytes = write_xlsx(&parse(BUDGET).unwrap()).unwrap();
        let text = read(&bytes, false).unwrap();
        assert!(text.starts_with("## Sheet: Budget\n\n| | A | B | C |\n|---|---|---|---|\n| 1 | Item | Cost | Share |\n| 2 | Rent | 1200 | =B2/B4 |\n| 3 | Food | 450.5 | =B3/B4 |\n| 4 | Total | =SUM(B2:B3) | 1 |"), "{text}");
        assert!(text.contains("## Sheet: Notes\n\n| | A | B | C |\n|---|---|---|---|\n| 1 | 00123 | | 2024-01-31 |\n| 3 | a \\| b | | TRUE |"), "{text}");
        assert!(
            !text.contains("Formula values"),
            "no stale cached results: {text}"
        );
        let again = parse(&text).unwrap();
        assert_eq!(again[0].cells.len(), 12);
    }
}
