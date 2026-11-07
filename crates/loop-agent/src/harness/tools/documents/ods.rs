//! OpenDocument spreadsheets (.ods) written from the spreadsheet format;
//! reading goes through calamine in `sheet`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::formula;
use super::sheet::{Cell, Sheet, Value};
use super::xml::{self, escape};

/// Cell styles: bold or not, times the number formats a value can need.
const KINDS: [&str; 5] = ["", "Date", "DateTime", "Percent", "Percent2"];

pub(super) fn write(sheets: &[Sheet]) -> Result<Vec<u8>, String> {
    let mut body = String::new();
    for sheet in sheets {
        table(&mut body, sheet);
    }
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-content {NS} office:version=\"1.2\"><office:automatic-styles>{}</office:automatic-styles><office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>",
        styles()
    );
    xml::package(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        ("META-INF/manifest.xml", MANIFEST),
        ("content.xml", &content),
    ])
}

fn table(out: &mut String, sheet: &Sheet) {
    let mut rows: BTreeMap<u32, Vec<&Cell>> = BTreeMap::new();
    for cell in &sheet.cells {
        rows.entry(cell.row).or_default().push(cell);
    }
    let columns = sheet
        .cells
        .iter()
        .map(|c| c.col as u32 + 1)
        .max()
        .unwrap_or(1);
    let _ = write!(
        out,
        "<table:table table:name=\"{}\"><table:table-column table:number-columns-repeated=\"{columns}\"/>",
        escape(&sheet.name)
    );
    let mut next_row = 0;
    for (row, mut cells) in rows {
        if row > next_row {
            let _ = write!(
                out,
                "<table:table-row table:number-rows-repeated=\"{}\"><table:table-cell table:number-columns-repeated=\"{columns}\"/></table:table-row>",
                row - next_row
            );
        }
        cells.sort_by_key(|c| c.col);
        out.push_str("<table:table-row>");
        let mut next_col = 0;
        for cell in cells {
            let gap = cell.col as u32 - next_col;
            match gap {
                0 => {}
                1 => out.push_str("<table:table-cell/>"),
                n => {
                    let _ = write!(
                        out,
                        "<table:table-cell table:number-columns-repeated=\"{n}\"/>"
                    );
                }
            }
            table_cell(out, cell);
            next_col = cell.col as u32 + 1;
        }
        out.push_str("</table:table-row>");
        next_row = row + 1;
    }
    if next_row == 0 {
        out.push_str("<table:table-row><table:table-cell/></table:table-row>");
    }
    out.push_str("</table:table>");
}

fn table_cell(out: &mut String, cell: &Cell) {
    let kind = match &cell.value {
        Value::Date(d) if d.time.is_some() => "DateTime",
        Value::Date(_) => "Date",
        Value::Percent(n) if (n * 100.0).fract() == 0.0 => "Percent",
        Value::Percent(_) => "Percent2",
        _ => "",
    };
    let style = if cell.bold || !kind.is_empty() {
        format!(
            " table:style-name=\"ce{}{kind}\"",
            if cell.bold { "Bold" } else { "" }
        )
    } else {
        String::new()
    };
    match &cell.value {
        Value::Text(text) => {
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"string\"{style}>"
            );
            for line in text.split('\n') {
                let _ = write!(out, "<text:p>{}</text:p>", escape(line));
            }
            out.push_str("</table:table-cell>");
        }
        Value::Number(n) => {
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"float\" office:value=\"{n}\"{style}><text:p>{n}</text:p></table:table-cell>"
            );
        }
        Value::Percent(n) => {
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"percentage\" office:value=\"{n}\"{style}><text:p>{}%</text:p></table:table-cell>",
                n * 100.0
            );
        }
        Value::Bool(b) => {
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"boolean\" office:boolean-value=\"{b}\"{style}><text:p>{}</text:p></table:table-cell>",
                if *b { "TRUE" } else { "FALSE" }
            );
        }
        Value::Date(date) => {
            let iso = date.iso();
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"date\" office:date-value=\"{iso}\"{style}><text:p>{}</text:p></table:table-cell>",
                iso.replace('T', " ")
            );
        }
        // No cached result: the spreadsheet app computes it on open.
        Value::Formula(f) => {
            let _ = write!(
                out,
                "<table:table-cell table:formula=\"{}\"{style}/>",
                escape(&formula::to_open_formula(f))
            );
        }
    }
}

fn styles() -> String {
    let mut out = String::from(
        "<number:date-style style:name=\"NDate\"><number:year number:style=\"long\"/><number:text>-</number:text><number:month number:style=\"long\"/><number:text>-</number:text><number:day number:style=\"long\"/></number:date-style>\
<number:date-style style:name=\"NDateTime\"><number:year number:style=\"long\"/><number:text>-</number:text><number:month number:style=\"long\"/><number:text>-</number:text><number:day number:style=\"long\"/><number:text> </number:text><number:hours number:style=\"long\"/><number:text>:</number:text><number:minutes number:style=\"long\"/><number:text>:</number:text><number:seconds number:style=\"long\"/></number:date-style>\
<number:percentage-style style:name=\"NPercent\"><number:number number:decimal-places=\"0\" number:min-integer-digits=\"1\"/><number:text>%</number:text></number:percentage-style>\
<number:percentage-style style:name=\"NPercent2\"><number:number number:decimal-places=\"2\" number:min-integer-digits=\"1\"/><number:text>%</number:text></number:percentage-style>",
    );
    for bold in [false, true] {
        for kind in KINDS {
            if !bold && kind.is_empty() {
                continue;
            }
            let name = format!("ce{}{kind}", if bold { "Bold" } else { "" });
            let data = if kind.is_empty() {
                String::new()
            } else {
                format!(" style:data-style-name=\"N{kind}\"")
            };
            let text = if bold {
                "<style:text-properties fo:font-weight=\"bold\"/>"
            } else {
                ""
            };
            let _ = write!(
                out,
                "<style:style style:name=\"{name}\" style:family=\"table-cell\"{data}>{text}</style:style>"
            );
        }
    }
    out
}

const NS: &str = "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" xmlns:number=\"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0\" xmlns:of=\"urn:oasis:names:tc:opendocument:xmlns:of:1.2\"";

const MANIFEST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.2\"><manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.2\" manifest:media-type=\"application/vnd.oasis.opendocument.spreadsheet\"/><manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/></manifest:manifest>";

#[cfg(test)]
mod tests {
    use super::super::sheet;
    use super::*;

    #[test]
    fn ods_round_trips_with_formulas() {
        let source = "## Sheet: Data\n\n| | A | B |\n|---|---|---|\n| 1 | **Name** | **Score** |\n| 2 | Ann | 7 |\n| 4 | Total | =SUM(B2:B3) |\n\n## Sheet: My Summary\n\n| | B |\n|---|---|\n| 2 | =Data!B4*2 |";
        let bytes = write(&sheet::parse(source).unwrap()).unwrap();
        let text = sheet::read(&bytes, true).unwrap();
        assert_eq!(
            text,
            "## Sheet: Data\n\n| | A | B |\n|---|---|---|\n| 1 | Name | Score |\n| 2 | Ann | 7 |\n| 4 | Total | =SUM(B2:B3) |\n\n## Sheet: My Summary\n\n| | B |\n|---|---|\n| 2 | =Data!B4*2 |"
        );
    }
}
