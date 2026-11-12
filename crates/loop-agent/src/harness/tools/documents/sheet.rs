//! The spreadsheet text format shared by `read` and `write` (see
//! `WRITE_GUIDE`), .xlsx/.ods reading through calamine, and .xlsx writing.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::LazyLock;

use calamine::{Data, Reader as _};
use regex::Regex;
use rust_xlsxwriter::{ExcelDateTime, Format, Formula, Workbook};

use super::markdown::table_cell;
use super::{formula, xml, OUTPUT_BUDGET};

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

/// The most cells a sheet's used area may span, and the most cells a
/// reader may hold for one sheet.
pub(super) const MAX_CELLS: u64 = 5_000_000;

/// (row, col) -> (cell text, value saved for a formula)
type Cells = BTreeMap<(u32, u32), (String, Option<String>)>;

/// The workbook in `bytes` (.xlsx or .ods) in the spreadsheet format.
///
/// calamine builds each sheet as a dense rows × columns array, so a sheet
/// with one cell at A1 and one at XFD1048576 would need 17 billion cells.
/// An .xlsx is therefore read cell by cell into a sparse map, and an .ods
/// (which calamine expands while opening) is measured first. Both are
/// fully inflated once with a budget before calamine sees them.
pub(super) fn read(bytes: &[u8], ods: bool) -> Result<String, String> {
    xml::Package::open(bytes)?.check_all()?;
    if ods {
        check_ods(bytes)?;
        read_ods(bytes)
    } else {
        read_xlsx(bytes)
    }
}

fn read_xlsx(bytes: &[u8]) -> Result<String, String> {
    let mut book: calamine::Xlsx<_> = calamine::Reader::new(Cursor::new(bytes))
        .map_err(|e| format!("could not open the spreadsheet: {e}"))?;
    let mut out = String::new();
    for name in book.sheet_names() {
        let mut cells = Cells::new();
        let mut reader = match book.worksheet_cells_reader(&name) {
            Ok(reader) => Some(reader),
            Err(calamine::XlsxError::NotAWorksheet(_)) => None,
            Err(e) => return Err(format!("sheet `{name}`: {e}")),
        };
        let mut count: u64 = 0;
        while let Some(reader) = reader.as_mut() {
            let Some(cell) = reader
                .next_cell_with_formula()
                .map_err(|e| format!("sheet `{name}`: {e}"))?
            else {
                break;
            };
            count += 1;
            if count > MAX_CELLS {
                return Err(too_many_cells(&name));
            }
            let value = value_text(&Data::from(cell.value));
            let formula = cell.formula.filter(|f| !f.trim().is_empty());
            let entry = match formula {
                Some(f) => (
                    format!("={}", formula::from_xlsx(&f)),
                    value.map(|v| v.trim_start_matches('\'').to_owned()),
                ),
                None => match value {
                    Some(v) => (v, None),
                    None => continue,
                },
            };
            cells.insert(cell.pos, entry);
        }
        check_span(&name, &cells)?;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        render_sheet(&mut out, &name, cells);
        if out.len() > OUTPUT_BUDGET {
            break;
        }
    }
    Ok(out)
}

fn read_ods(bytes: &[u8]) -> Result<String, String> {
    let mut book: calamine::Ods<_> = calamine::Reader::new(Cursor::new(bytes))
        .map_err(|e| format!("could not open the spreadsheet: {e}"))?;
    let mut out = String::new();
    for name in book.sheet_names() {
        let values = book
            .worksheet_range(&name)
            .map_err(|e| format!("sheet `{name}`: {e}"))?;
        let formulas = book.worksheet_formula(&name).ok();
        let mut cells = Cells::new();
        let (row0, col0) = values.start().unwrap_or((0, 0));
        for (r, c, value) in values.cells() {
            if let Some(text) = value_text(value) {
                cells.insert((row0 + r as u32, col0 + c as u32), (text, None));
            }
        }
        if let Some(formulas) = &formulas {
            let (frow0, fcol0) = formulas.start().unwrap_or((0, 0));
            for (r, c, stored) in formulas.cells() {
                if stored.trim().is_empty() {
                    continue;
                }
                let at = (frow0 + r as u32, fcol0 + c as u32);
                let saved = cells
                    .remove(&at)
                    .map(|(text, _)| text.trim_start_matches('\'').to_owned());
                cells.insert(at, (format!("={}", formula::to_excel(stored)), saved));
            }
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        render_sheet(&mut out, &name, cells);
        if out.len() > OUTPUT_BUDGET {
            break;
        }
    }
    Ok(out)
}

fn too_many_cells(sheet: &str) -> String {
    format!("sheet `{sheet}` has more than {MAX_CELLS} cells, too many to read")
}

/// Refuse a sheet whose used area spans more than [`MAX_CELLS`] cells.
fn check_span(sheet: &str, cells: &Cells) -> Result<(), String> {
    let (Some(first), Some(last)) = (cells.keys().next(), cells.keys().next_back()) else {
        return Ok(());
    };
    let rows = u64::from(last.0 - first.0) + 1;
    let min_col = cells.keys().map(|(_, c)| *c).min().unwrap_or(0);
    let max_col = cells.keys().map(|(_, c)| *c).max().unwrap_or(0);
    let cols = u64::from(max_col - min_col) + 1;
    if rows.saturating_mul(cols) > MAX_CELLS {
        return Err(format!(
            "sheet `{sheet}` spans {rows} rows × {cols} columns, more than the {MAX_CELLS} cells \
             that can be read"
        ));
    }
    Ok(())
}

/// Measure the tables of an .ods before calamine expands them: the cells
/// it would hold while reading, and the area of its dense result.
fn check_ods(bytes: &[u8]) -> Result<(), String> {
    const MAX_ROWS: u64 = 1_048_576;
    const MAX_COLS: u64 = 16_384;
    let mut package = xml::Package::open(bytes)?;
    let Some(content) = package.text("content.xml")? else {
        return Ok(());
    };
    #[derive(Default)]
    struct Table {
        name: String,
        /// Expanded rows so far.
        row: u64,
        /// Cells calamine stores while reading (before trimming).
        stored: u64,
        rows: Option<(u64, u64)>,
        max_col: u64,
        min_col: Option<u64>,
    }
    let mut table: Option<Table> = None;
    let (mut row_repeat, mut col, mut row_last) = (1u64, 0u64, 0u64);
    let mut row_min_col: Option<u64> = None;
    // The cell being read: its start column, repeat, whether it has content.
    let mut cell: Option<(u64, u64, bool)> = None;
    let number = |attrs: &[(String, String)], name: &str, max: u64| {
        xml::attr(attrs, name)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .clamp(1, max)
    };
    for node in xml::nodes(&content).in_part("content.xml") {
        match node? {
            xml::Node::Open { name, attrs } => match name.as_str() {
                "table" => {
                    table = Some(Table {
                        name: xml::attr(&attrs, "name").unwrap_or_default().to_owned(),
                        ..Table::default()
                    })
                }
                "table-row" => {
                    row_repeat = number(&attrs, "number-rows-repeated", MAX_ROWS);
                    (col, row_last, row_min_col) = (0, 0, None);
                }
                "table-cell" | "covered-table-cell" => {
                    let repeat = number(&attrs, "number-columns-repeated", MAX_COLS);
                    let valued = xml::attr(&attrs, "value-type").is_some()
                        || xml::attr(&attrs, "formula").is_some();
                    cell = Some((col, repeat, valued));
                }
                _ => {
                    if let Some(c) = &mut cell {
                        c.2 |= name == "p" || name == "s";
                    }
                }
            },
            xml::Node::Text(text) => {
                if let Some(c) = &mut cell {
                    c.2 |= !text.trim().is_empty();
                }
            }
            xml::Node::Close(name) => match name.as_str() {
                "table-cell" | "covered-table-cell" => {
                    if let Some((start, repeat, filled)) = cell.take() {
                        col = (start + repeat).min(MAX_COLS);
                        if filled {
                            row_last = col;
                            row_min_col.get_or_insert(start);
                        }
                    }
                }
                "table-row" => {
                    if let Some(t) = &mut table {
                        let first = t.row;
                        t.row = (t.row + row_repeat).min(MAX_ROWS);
                        t.stored += row_last;
                        if let Some(min) = row_min_col {
                            let (lo, _) = t.rows.unwrap_or((first, 0));
                            t.rows = Some((lo, t.row - 1));
                            t.max_col = t.max_col.max(row_last);
                            t.min_col = Some(t.min_col.map_or(min, |m| m.min(min)));
                        }
                        let area = match (t.rows, t.min_col) {
                            (Some((lo, hi)), Some(min)) => (hi - lo + 1) * (t.max_col - min),
                            _ => 0,
                        };
                        if t.stored > MAX_CELLS || area > MAX_CELLS {
                            return Err(too_many_cells(&t.name));
                        }
                    }
                }
                "table" => table = None,
                _ => {}
            },
        }
    }
    Ok(())
}

fn render_sheet(out: &mut String, name: &str, cells: Cells) {
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
        if out.len() > OUTPUT_BUDGET {
            return;
        }
    }
    out.push_str("\n|---|");
    for _ in first_col..=last_col {
        out.push_str("---|");
        if out.len() > OUTPUT_BUDGET {
            return;
        }
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
        if out.len() > OUTPUT_BUDGET {
            return;
        }
    }
    let mut saved = cells
        .iter()
        .filter_map(|((row, col), (_, saved))| Some((*row, *col, saved.as_ref()?)))
        .peekable();
    if saved.peek().is_some() {
        out.push_str(
            "\n\nFormula values saved in the file (recalculated when opened; ignored by write):",
        );
        for (row, col, value) in saved {
            let _ = write!(
                out,
                "\n- {}: {}",
                address(row, col as u16),
                value.replace('\n', " ")
            );
            if out.len() > OUTPUT_BUDGET {
                return;
            }
        }
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
