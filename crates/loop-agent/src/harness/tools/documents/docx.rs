//! Word documents (.docx): WordprocessingML to markdown and back.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::markdown::{self, Block, Markdown, Marker, Run, Span};
use super::xml::{self, attr, escape, Node, Package};

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    let mut package = Package::open(bytes)?;
    let document = package
        .text("word/document.xml")?
        .ok_or("not a Word document: word/document.xml is missing")?;
    let headings = match package.text("word/styles.xml")? {
        Some(styles) => heading_styles(&styles)?,
        None => HashMap::new(),
    };
    let ordered = match package.text("word/numbering.xml")? {
        Some(numbering) => ordered_levels(&numbering)?,
        None => HashMap::new(),
    };
    let mut reader = Reader {
        headings,
        ordered,
        ..Default::default()
    };
    for node in xml::nodes(&document)? {
        reader.node(node);
    }
    Ok(reader.md.finish())
}

/// Heading level by style id, from the style names (`heading 2`, `Title`)
/// and outline levels in styles.xml.
fn heading_styles(styles: &str) -> Result<HashMap<String, u8>, String> {
    let mut out = HashMap::new();
    let mut id: Option<String> = None;
    for node in xml::nodes(styles)? {
        let Node::Open { name, attrs } = node else {
            continue;
        };
        match name.as_str() {
            "style" => id = attr(&attrs, "styleId").map(str::to_owned),
            "name" | "outlineLvl" => {
                let (Some(id), Some(val)) = (&id, attr(&attrs, "val")) else {
                    continue;
                };
                let level = if name == "outlineLvl" {
                    val.parse::<u8>().ok().filter(|l| *l < 9).map(|l| l + 1)
                } else {
                    let val = val.to_ascii_lowercase();
                    if val == "title" {
                        Some(1)
                    } else {
                        val.strip_prefix("heading ").and_then(|n| n.parse().ok())
                    }
                };
                if let Some(level) = level {
                    out.entry(id.clone()).or_insert(level);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Whether each (numId, level) is a numbered list rather than bullets.
fn ordered_levels(numbering: &str) -> Result<HashMap<(String, u8), bool>, String> {
    let mut abstracts: HashMap<String, HashMap<u8, bool>> = HashMap::new();
    let mut nums: Vec<(String, String)> = Vec::new();
    let (mut abstract_id, mut level, mut num_id) = (None::<String>, 0u8, None::<String>);
    for node in xml::nodes(numbering)? {
        let Node::Open { name, attrs } = node else {
            continue;
        };
        match name.as_str() {
            "abstractNum" => abstract_id = attr(&attrs, "abstractNumId").map(str::to_owned),
            "lvl" => {
                level = attr(&attrs, "ilvl")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            }
            "numFmt" => {
                if let (Some(id), Some(fmt)) = (&abstract_id, attr(&attrs, "val")) {
                    let ordered = !matches!(fmt, "bullet" | "none");
                    abstracts
                        .entry(id.clone())
                        .or_default()
                        .insert(level, ordered);
                }
            }
            "num" => {
                abstract_id = None;
                num_id = attr(&attrs, "numId").map(str::to_owned);
            }
            "abstractNumId" => {
                if let (Some(num), Some(target)) = (num_id.take(), attr(&attrs, "val")) {
                    nums.push((num, target.to_owned()));
                }
            }
            _ => {}
        }
    }
    let mut out = HashMap::new();
    for (num, target) in nums {
        for (level, ordered) in abstracts.get(&target).into_iter().flatten() {
            out.insert((num.clone(), *level), *ordered);
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Paragraph {
    style: Option<String>,
    num: Option<String>,
    level: u8,
    outline: Option<u8>,
    runs: Vec<Run>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: Vec<String>,
}

#[derive(Default)]
struct Reader {
    headings: HashMap<String, u8>,
    ordered: HashMap<(String, u8), bool>,
    md: Markdown,
    /// Open paragraphs; text boxes nest paragraphs inside paragraphs.
    paragraphs: Vec<Paragraph>,
    tables: Vec<Table>,
    bold: bool,
    italic: bool,
    in_text: bool,
    in_props: bool,
    /// Depth inside content that duplicates or is not body text.
    skip: usize,
}

impl Reader {
    fn node(&mut self, node: Node) {
        match node {
            Node::Open { name, attrs } => self.open(&name, &attrs),
            Node::Close(name) => self.close(&name),
            Node::Text(text) => {
                if self.in_text && self.skip == 0 {
                    self.push(&text);
                }
            }
        }
    }

    fn open(&mut self, name: &str, attrs: &[(String, String)]) {
        if matches!(name, "Fallback" | "footnoteReference" | "commentReference") || self.skip > 0 {
            self.skip += 1;
            return;
        }
        match name {
            "p" => self.paragraphs.push(Paragraph::default()),
            "pPr" => self.in_props = true,
            "pStyle" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.style = attr(attrs, "val").map(str::to_owned);
                }
            }
            "numId" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.num = attr(attrs, "val").filter(|v| *v != "0").map(str::to_owned);
                }
            }
            "ilvl" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.level = attr(attrs, "val").and_then(|v| v.parse().ok()).unwrap_or(0);
                }
            }
            "outlineLvl" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.outline = attr(attrs, "val")
                        .and_then(|v| v.parse().ok())
                        .filter(|l| *l < 9);
                }
            }
            "r" => {
                self.bold = false;
                self.italic = false;
            }
            "b" if !self.in_props => self.bold = on(attrs),
            "i" if !self.in_props => self.italic = on(attrs),
            "t" => self.in_text = true,
            "tab" if !self.in_props => self.push("\t"),
            "br" | "cr" => self.push("\n"),
            "noBreakHyphen" => self.push("-"),
            "tbl" => self.tables.push(Table::default()),
            "tr" => {
                if let Some(t) = self.tables.last_mut() {
                    t.row.clear();
                }
            }
            "tc" => {
                if let Some(t) = self.tables.last_mut() {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        match name {
            "pPr" => self.in_props = false,
            "t" => self.in_text = false,
            "p" => {
                if let Some(p) = self.paragraphs.pop() {
                    self.paragraph(p);
                }
            }
            "tc" => {
                if let Some(t) = self.tables.last_mut() {
                    let cell = t.cell.join("\n");
                    t.row.push(cell);
                }
            }
            "tr" => {
                if let Some(t) = self.tables.last_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            "tbl" => {
                if let Some(table) = self.tables.pop() {
                    match self.tables.last_mut() {
                        Some(outer) => outer.cell.push(
                            table
                                .rows
                                .iter()
                                .map(|r| r.join(" / "))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        None => self.md.block(&markdown::table(&table.rows)),
                    }
                }
            }
            _ => {}
        }
    }

    fn push(&mut self, text: &str) {
        let (bold, italic) = (self.bold, self.italic);
        if let Some(p) = self.paragraphs.last_mut() {
            p.runs.push(Run {
                text: text.to_owned(),
                bold,
                italic,
            });
        }
    }

    fn paragraph(&mut self, p: Paragraph) {
        let heading = p
            .style
            .as_ref()
            .and_then(|s| self.headings.get(s).copied())
            .or(p.outline.map(|l| l + 1));
        if let Some(table) = self.tables.last_mut() {
            let text = markdown::inline(&p.runs);
            if !text.is_empty() {
                table.cell.push(text);
            }
            return;
        }
        if let Some(level) = heading {
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            self.md.heading(level, &text);
        } else if let Some(num) = &p.num {
            let ordered = self
                .ordered
                .get(&(num.clone(), p.level))
                .copied()
                .unwrap_or(false);
            self.md
                .item(p.level as usize, ordered, &markdown::inline(&p.runs));
        } else {
            self.md.block(&markdown::inline(&p.runs));
        }
    }
}

/// Whether a toggle property (`<w:b/>`, `<w:b w:val="0"/>`) is on.
fn on(attrs: &[(String, String)]) -> bool {
    !matches!(attr(attrs, "val"), Some("0" | "false" | "off" | "none"))
}

pub(super) fn write(blocks: &[Block]) -> Result<Vec<u8>, String> {
    let mut body = String::new();
    // numId 1 is bullets; each numbered list gets its own numId so it
    // restarts at its first number.
    let mut numbered: Vec<(usize, usize, u64)> = Vec::new();
    for block in blocks {
        match block {
            Block::Heading { level, spans } => {
                let style = format!("<w:pStyle w:val=\"Heading{}\"/>", (*level).clamp(1, 6));
                paragraph(&mut body, &style, spans);
            }
            Block::Paragraph(spans) => paragraph(&mut body, "", spans),
            Block::Quote(spans) => paragraph(&mut body, "<w:pStyle w:val=\"Quote\"/>", spans),
            Block::Item {
                depth,
                marker,
                list,
                spans,
            } => {
                let depth = (*depth).min(8);
                let props = match marker {
                    Marker::Bullet => list_props(depth, 1),
                    Marker::Number(n) => {
                        let id = match numbered.iter().find(|(l, _, _)| l == list) {
                            Some((_, id, _)) => *id,
                            None => {
                                let id = numbered.len() + 2;
                                numbered.push((*list, id, *n));
                                id
                            }
                        };
                        list_props(depth, id)
                    }
                    Marker::Continued => format!(
                        "<w:pStyle w:val=\"ListParagraph\"/><w:ind w:left=\"{}\"/>",
                        720 * (depth + 1)
                    ),
                };
                paragraph(&mut body, &props, spans);
            }
            Block::Code(code) => {
                let span = Span {
                    text: code.clone(),
                    ..Default::default()
                };
                paragraph(&mut body, "<w:pStyle w:val=\"Code\"/>", &[span]);
            }
            Block::Rule => body.push_str(
                "<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"auto\"/></w:pBdr></w:pPr></w:p>",
            ),
            Block::Table(rows) => table(&mut body, rows),
        }
    }
    let document = format!(
        "{XML_DECL}<w:document xmlns:w=\"{W_NS}\"><w:body>{body}<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"
    );
    xml::package(&[
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", PACKAGE_RELS),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS),
        ("word/document.xml", &document),
        ("word/styles.xml", &styles()),
        ("word/numbering.xml", &numbering(&numbered)),
    ])
}

fn list_props(depth: usize, num_id: usize) -> String {
    format!(
        "<w:pStyle w:val=\"ListParagraph\"/><w:numPr><w:ilvl w:val=\"{depth}\"/><w:numId w:val=\"{num_id}\"/></w:numPr>"
    )
}

fn paragraph(out: &mut String, props: &str, spans: &[Span]) {
    out.push_str("<w:p>");
    if !props.is_empty() {
        let _ = write!(out, "<w:pPr>{props}</w:pPr>");
    }
    runs(out, spans, false);
    out.push_str("</w:p>");
}

fn runs(out: &mut String, spans: &[Span], force_bold: bool) {
    for span in spans {
        let mut props = String::new();
        if span.code {
            props.push_str(
                "<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>",
            );
        }
        if span.bold || force_bold {
            props.push_str("<w:b/>");
        }
        if span.italic {
            props.push_str("<w:i/>");
        }
        let props = if props.is_empty() {
            String::new()
        } else {
            format!("<w:rPr>{props}</w:rPr>")
        };
        out.push_str("<w:r>");
        out.push_str(&props);
        for (i, line) in span.text.split('\n').enumerate() {
            if i > 0 {
                out.push_str("<w:br/>");
            }
            for (j, piece) in line.split('\t').enumerate() {
                if j > 0 {
                    out.push_str("<w:tab/>");
                }
                if !piece.is_empty() {
                    let _ = write!(out, "<w:t xml:space=\"preserve\">{}</w:t>", escape(piece));
                }
            }
        }
        out.push_str("</w:r>");
    }
}

fn table(out: &mut String, rows: &[Vec<Vec<Span>>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let width = 9360 / cols;
    out.push_str("<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/><w:tblW w:w=\"0\" w:type=\"auto\"/></w:tblPr><w:tblGrid>");
    for _ in 0..cols {
        let _ = write!(out, "<w:gridCol w:w=\"{width}\"/>");
    }
    out.push_str("</w:tblGrid>");
    for (i, row) in rows.iter().enumerate() {
        out.push_str("<w:tr>");
        if i == 0 {
            out.push_str("<w:trPr><w:tblHeader/></w:trPr>");
        }
        for c in 0..cols {
            let _ = write!(
                out,
                "<w:tc><w:tcPr><w:tcW w:w=\"{width}\" w:type=\"dxa\"/></w:tcPr><w:p>"
            );
            if let Some(cell) = row.get(c) {
                runs(out, cell, i == 0);
            }
            out.push_str("</w:p></w:tc>");
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl><w:p/>");
}

fn numbering(numbered: &[(usize, usize, u64)]) -> String {
    let mut out = format!("{XML_DECL}<w:numbering xmlns:w=\"{W_NS}\">");
    for (id, ordered) in [(0, false), (1, true)] {
        let _ = write!(out, "<w:abstractNum w:abstractNumId=\"{id}\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
        for level in 0..9usize {
            let (fmt, text) = if ordered {
                let fmt = ["decimal", "lowerLetter", "lowerRoman"][level % 3];
                (fmt, format!("%{}.", level + 1))
            } else {
                (
                    "bullet",
                    ["\u{2022}", "\u{25E6}", "\u{25AA}"][level % 3].to_owned(),
                )
            };
            let _ = write!(
                out,
                "<w:lvl w:ilvl=\"{level}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"{fmt}\"/><w:lvlText w:val=\"{text}\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"360\"/></w:pPr></w:lvl>",
                720 * (level + 1)
            );
        }
        out.push_str("</w:abstractNum>");
    }
    out.push_str("<w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num>");
    for (_, id, start) in numbered {
        let _ = write!(
            out,
            "<w:num w:numId=\"{id}\"><w:abstractNumId w:val=\"1\"/>"
        );
        for level in 0..9 {
            let start = if level == 0 { *start } else { 1 };
            let _ = write!(
                out,
                "<w:lvlOverride w:ilvl=\"{level}\"><w:startOverride w:val=\"{start}\"/></w:lvlOverride>"
            );
        }
        out.push_str("</w:num>");
    }
    out.push_str("</w:numbering>");
    out
}

fn styles() -> String {
    let mut out = format!(
        "{XML_DECL}<w:styles xmlns:w=\"{W_NS}\"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:eastAsia=\"Calibri\" w:cs=\"Calibri\"/><w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/><w:lang w:val=\"en-US\"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after=\"160\" w:line=\"259\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>"
    );
    for (level, size) in [(1, 32), (2, 28), (3, 24), (4, 22), (5, 22), (6, 22)] {
        let _ = write!(
            out,
            "<w:style w:type=\"paragraph\" w:styleId=\"Heading{level}\"><w:name w:val=\"heading {level}\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:uiPriority w:val=\"9\"/><w:qFormat/><w:pPr><w:keepNext/><w:keepLines/><w:spacing w:before=\"{}\" w:after=\"80\"/><w:outlineLvl w:val=\"{}\"/></w:pPr><w:rPr><w:b/><w:color w:val=\"1F3864\"/><w:sz w:val=\"{size}\"/><w:szCs w:val=\"{size}\"/></w:rPr></w:style>",
            if level <= 2 { 360 } else { 160 },
            level - 1
        );
    }
    out.push_str(
        "<w:style w:type=\"paragraph\" w:styleId=\"Quote\"><w:name w:val=\"Quote\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/><w:pPr><w:ind w:left=\"720\" w:right=\"720\"/></w:pPr><w:rPr><w:i/><w:color w:val=\"404040\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Code\"><w:name w:val=\"Code\"/><w:basedOn w:val=\"Normal\"/><w:pPr><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F2F2F2\"/><w:spacing w:after=\"160\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr><w:rPr><w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/><w:sz w:val=\"20\"/><w:szCs w:val=\"20\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"ListParagraph\"><w:name w:val=\"List Paragraph\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/><w:pPr><w:spacing w:after=\"60\"/><w:ind w:left=\"720\"/><w:contextualSpacing/></w:pPr></w:style>\
<w:style w:type=\"table\" w:default=\"1\" w:styleId=\"TableNormal\"><w:name w:val=\"Normal Table\"/><w:tblPr><w:tblInd w:w=\"0\" w:type=\"dxa\"/><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"108\" w:type=\"dxa\"/><w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"108\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style>\
<w:style w:type=\"table\" w:styleId=\"TableGrid\"><w:name w:val=\"Table Grid\"/><w:basedOn w:val=\"TableNormal\"/><w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr><w:tblPr><w:tblBorders><w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/></w:tblBorders></w:tblPr></w:style>\
</w:styles>",
    );
    out
}

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

const CONTENT_TYPES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/><Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/><Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/></Types>";

const PACKAGE_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>";

const DOCUMENT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/></Relationships>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_round_trips_through_docx() {
        let source = "# Report\n\nIntro with **bold** and *italic*.\n\n## Steps\n\n1. First\n2. Second\n    - detail\n\n| Name | Qty |\n| --- | --- |\n| Apple | 3 |";
        let bytes = write(&markdown::parse(source)).unwrap();
        let text = read(&bytes).unwrap();
        assert_eq!(text, "# Report\n\nIntro with **bold** and *italic*.\n\n## Steps\n\n1. First\n2. Second\n    - detail\n\n| **Name** | **Qty** |\n| --- | --- |\n| Apple | 3 |");
    }
}
