//! PowerPoint decks (.pptx): one `# Title` section per slide, both ways.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::markdown::{self, Block, Markdown, Marker, Run, Span};
use super::xml::{self, attr, escape, Node, Package};

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    let mut package = Package::open(bytes)?;
    let presentation = package
        .text("ppt/presentation.xml")?
        .ok_or("not a PowerPoint file: ppt/presentation.xml is missing")?;
    let rels = match package.text("ppt/_rels/presentation.xml.rels")? {
        Some(rels) => xml::relationships(&rels)?,
        None => HashMap::new(),
    };
    let mut md = Markdown::default();
    let mut number = 0;
    for node in xml::nodes(&presentation)? {
        let Node::Open { name, attrs } = node else {
            continue;
        };
        if name != "sldId" {
            continue;
        }
        let Some(target) = attr(&attrs, "r:id").and_then(|id| rels.get(id)) else {
            continue;
        };
        let Some(slide) = package.text(&xml::resolve("ppt/", target))? else {
            continue;
        };
        number += 1;
        read_slide(&mut md, number, &slide)?;
    }
    Ok(md.finish())
}

#[derive(Default)]
struct Paragraph {
    level: usize,
    /// `Some(ordered)` when the paragraph sets its own bullet.
    bullet: Option<bool>,
    no_bullet: bool,
    runs: Vec<Run>,
}

#[derive(Default)]
struct Shape {
    title: bool,
    /// A content placeholder, whose paragraphs are bullets by default.
    body: bool,
    paragraphs: Vec<Paragraph>,
}

enum Content {
    Text(Shape),
    Table(Vec<Vec<String>>),
}

fn read_slide(md: &mut Markdown, number: usize, slide: &str) -> Result<(), String> {
    let mut title: Option<String> = None;
    let mut content = Vec::new();
    let mut shape: Option<Shape> = None;
    let mut paragraph: Option<Paragraph> = None;
    let (mut bold, mut italic, mut in_text) = (false, false, false);
    let mut table: Option<Vec<Vec<String>>> = None;
    let (mut row, mut cell) = (Vec::new(), Vec::new());
    for node in xml::nodes(slide)? {
        match node {
            Node::Open { name, attrs } => match name.as_str() {
                "sp" => shape = Some(Shape::default()),
                "ph" => {
                    if let Some(s) = &mut shape {
                        let kind = attr(&attrs, "type");
                        s.title = matches!(kind, Some("title" | "ctrTitle"));
                        s.body = matches!(kind, None | Some("body" | "obj"));
                    }
                }
                "p" => paragraph = Some(Paragraph::default()),
                "pPr" => {
                    if let Some(p) = &mut paragraph {
                        p.level = attr(&attrs, "lvl")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                    }
                }
                "buNone" => {
                    if let Some(p) = &mut paragraph {
                        p.no_bullet = true;
                    }
                }
                "buChar" | "buAutoNum" => {
                    if let Some(p) = &mut paragraph {
                        p.bullet = Some(name == "buAutoNum");
                    }
                }
                "r" | "fld" => (bold, italic) = (false, false),
                "rPr" => {
                    bold = matches!(attr(&attrs, "b"), Some("1" | "true"));
                    italic = matches!(attr(&attrs, "i"), Some("1" | "true"));
                }
                "t" => in_text = true,
                "br" => {
                    if let Some(p) = &mut paragraph {
                        p.runs.push(Run {
                            text: "\n".into(),
                            bold,
                            italic,
                        });
                    }
                }
                "tbl" => table = Some(Vec::new()),
                "tr" => row.clear(),
                "tc" => cell.clear(),
                _ => {}
            },
            Node::Close(name) => match name.as_str() {
                "t" => in_text = false,
                "p" => {
                    let Some(p) = paragraph.take() else { continue };
                    if table.is_some() {
                        cell.push(markdown::inline(&p.runs));
                    } else if let Some(s) = &mut shape {
                        s.paragraphs.push(p);
                    }
                }
                "tc" => row.push(cell.join("\n")),
                "tr" => {
                    if let Some(t) = &mut table {
                        t.push(std::mem::take(&mut row));
                    }
                }
                "tbl" => {
                    if let Some(t) = table.take() {
                        content.push(Content::Table(t));
                    }
                }
                "sp" => {
                    let Some(s) = shape.take() else { continue };
                    if s.title && title.is_none() {
                        let text: Vec<String> = s
                            .paragraphs
                            .iter()
                            .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect())
                            .collect();
                        title = Some(
                            text.join(" ")
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    } else {
                        content.push(Content::Text(s));
                    }
                }
                _ => {}
            },
            Node::Text(text) => {
                if in_text {
                    if let Some(p) = &mut paragraph {
                        p.runs.push(Run { text, bold, italic });
                    }
                }
            }
        }
    }
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("Slide {number}"));
    md.heading(1, &title);
    for item in content {
        match item {
            Content::Text(shape) => {
                for p in shape.paragraphs {
                    let text = markdown::inline(&p.runs);
                    let bulleted = !p.no_bullet && (p.bullet.is_some() || shape.body);
                    if bulleted {
                        md.item(p.level, p.bullet == Some(true), &text);
                    } else {
                        md.block(&text);
                    }
                }
            }
            Content::Table(rows) => md.block(&markdown::table(&rows)),
        }
    }
    Ok(())
}

const SLIDE_W: i64 = 12_192_000;
const SLIDE_H: i64 = 6_858_000;
const LEFT: i64 = 838_200;
const WIDTH: i64 = 10_515_600;
const TITLE_Y: i64 = 365_125;
const BODY_Y: i64 = 1_825_625;
const ROW_H: i64 = 370_840;

struct Slide<'a> {
    title: Option<&'a [Span]>,
    body: Vec<&'a Block>,
}

pub(super) fn write(blocks: &[Block]) -> Result<Vec<u8>, String> {
    let slides = split(blocks);
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut ids = String::new();
    let mut rels = String::from(
        "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps\" Target=\"presProps.xml\"/>\
<Relationship Id=\"rId4\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps\" Target=\"viewProps.xml\"/>\
<Relationship Id=\"rId5\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles\" Target=\"tableStyles.xml\"/>",
    );
    let mut types = String::new();
    for (i, slide) in slides.iter().enumerate() {
        let n = i + 1;
        let _ = write!(ids, "<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 256 + i, 10 + i);
        let _ = write!(
            rels,
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{n}.xml\"/>",
            10 + i
        );
        let _ = write!(
            types,
            "<Override PartName=\"/ppt/slides/slide{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>"
        );
        parts.push((format!("ppt/slides/slide{n}.xml"), slide_xml(slide)));
        parts.push((
            format!("ppt/slides/_rels/slide{n}.xml.rels"),
            rels_xml(SLIDE_RELS),
        ));
    }
    let presentation = format!(
        "{XML_DECL}<p:presentation {NS} saveSubsetFonts=\"1\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"{SLIDE_W}\" cy=\"{SLIDE_H}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/><p:defaultTextStyle><a:defPPr><a:defRPr lang=\"en-US\"/></a:defPPr></p:defaultTextStyle></p:presentation>"
    );
    let content_types = format!(
        "{XML_DECL}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/>\
<Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/>\
<Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>\
<Override PartName=\"/ppt/presProps.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presProps+xml\"/>\
<Override PartName=\"/ppt/viewProps.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml\"/>\
<Override PartName=\"/ppt/tableStyles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml\"/>{types}</Types>"
    );
    let mut all: Vec<(&str, String)> = vec![
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels_xml(PACKAGE_RELS)),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", rels_xml(&rels)),
        ("ppt/slideMasters/slideMaster1.xml", master_xml()),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            rels_xml(MASTER_RELS),
        ),
        ("ppt/slideLayouts/slideLayout1.xml", layout_xml()),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            rels_xml(LAYOUT_RELS),
        ),
        ("ppt/theme/theme1.xml", format!("{XML_DECL}{THEME}")),
        (
            "ppt/presProps.xml",
            format!("{XML_DECL}<p:presentationPr {NS}/>"),
        ),
        ("ppt/viewProps.xml", format!("{XML_DECL}<p:viewPr {NS}/>")),
        (
            "ppt/tableStyles.xml",
            format!("{XML_DECL}<a:tblStyleLst xmlns:a=\"{A_NS}\" def=\"{TABLE_STYLE}\"/>"),
        ),
    ];
    for (name, xml) in &parts {
        all.push((name.as_str(), xml.clone()));
    }
    let refs: Vec<(&str, &str)> = all.iter().map(|(n, x)| (*n, x.as_str())).collect();
    xml::package(&refs)
}

/// Slides from the blocks: each top-level heading starts a slide and is its
/// title; a horizontal rule starts an untitled one.
fn split(blocks: &[Block]) -> Vec<Slide<'_>> {
    let top = blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading { level, .. } => Some(*level),
            _ => None,
        })
        .min();
    let mut slides: Vec<Slide> = Vec::new();
    for block in blocks {
        match block {
            Block::Heading { level, spans } if Some(*level) == top => match slides.last_mut() {
                Some(last) if last.title.is_none() && last.body.is_empty() => {
                    last.title = Some(spans)
                }
                _ => slides.push(Slide {
                    title: Some(spans),
                    body: Vec::new(),
                }),
            },
            Block::Rule => slides.push(Slide {
                title: None,
                body: Vec::new(),
            }),
            other => match slides.last_mut() {
                Some(last) => last.body.push(other),
                None => slides.push(Slide {
                    title: None,
                    body: vec![other],
                }),
            },
        }
    }
    slides.retain(|s| s.title.is_some() || !s.body.is_empty());
    if slides.is_empty() {
        slides.push(Slide {
            title: None,
            body: Vec::new(),
        });
    }
    slides
}

fn slide_xml(slide: &Slide) -> String {
    let mut shapes = String::new();
    let mut id = 2;
    if let Some(title) = slide.title {
        let paragraph = format!("<a:p>{}</a:p>", runs(title, false, false, None));
        shapes.push_str(&placeholder(
            id,
            "Title",
            "<p:ph type=\"title\"/>",
            "",
            "<a:bodyPr/>",
            &paragraph,
        ));
        id += 1;
    }
    let tables: Vec<&Vec<Vec<Vec<Span>>>> = slide
        .body
        .iter()
        .filter_map(|b| match b {
            Block::Table(rows) => Some(rows),
            _ => None,
        })
        .collect();
    let (paragraphs, count) = body_paragraphs(&slide.body);
    let top = if slide.title.is_some() {
        BODY_Y
    } else {
        TITLE_Y
    };
    let available = SLIDE_H - top - 400_000;
    let text_height = match (count, tables.is_empty()) {
        (0, _) => 0,
        (_, true) => available,
        (n, false) => (n as i64 * 420_000 + 200_000).min(available / 2),
    };
    if count > 0 {
        let frame = xfrm(LEFT, top, WIDTH, text_height);
        shapes.push_str(&placeholder(
            id,
            "Content",
            "<p:ph idx=\"1\"/>",
            &frame,
            "<a:bodyPr><a:normAutofit/></a:bodyPr>",
            &paragraphs,
        ));
        id += 1;
    }
    let mut y = top + text_height + if count > 0 { 150_000 } else { 0 };
    for rows in tables {
        shapes.push_str(&table_frame(id, rows, y));
        y += rows.len() as i64 * ROW_H + 150_000;
        id += 1;
    }
    format!(
        "{XML_DECL}<p:sld {NS}><p:cSld><p:spTree>{GROUP}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )
}

/// The body paragraphs of a slide and how many there are; tables go in
/// their own frames.
fn body_paragraphs(blocks: &[&Block]) -> (String, usize) {
    const PLAIN: &str = "<a:pPr marL=\"0\" indent=\"0\"><a:buNone/></a:pPr>";
    let mut out = String::new();
    let mut count = 0;
    let mut add = |props: &str, runs: String| {
        let _ = write!(out, "<a:p>{props}{runs}</a:p>");
        count += 1;
    };
    for block in blocks {
        match block {
            Block::Paragraph(spans) => add(PLAIN, runs(spans, false, false, None)),
            Block::Heading { spans, .. } => add(PLAIN, runs(spans, true, false, None)),
            Block::Quote(spans) => add(PLAIN, runs(spans, false, true, None)),
            Block::Item {
                depth,
                marker,
                spans,
                ..
            } => {
                let level = (*depth).min(8);
                let props = match marker {
                    Marker::Bullet => format!("<a:pPr lvl=\"{level}\"/>"),
                    Marker::Number(_) => format!(
                        "<a:pPr lvl=\"{level}\"><a:buFont typeface=\"+mj-lt\"/><a:buAutoNum type=\"arabicPeriod\"/></a:pPr>"
                    ),
                    Marker::Continued => format!("<a:pPr lvl=\"{level}\" indent=\"0\"><a:buNone/></a:pPr>"),
                };
                add(&props, runs(spans, false, false, None));
            }
            Block::Code(code) => {
                for line in code.lines() {
                    let span = Span {
                        text: line.to_owned(),
                        code: true,
                        ..Default::default()
                    };
                    add(PLAIN, runs(&[span], false, false, None));
                }
            }
            Block::Table(_) | Block::Rule => {}
        }
    }
    (out, count)
}

fn runs(spans: &[Span], bold: bool, italic: bool, size: Option<u32>) -> String {
    let mut out = String::new();
    for span in spans {
        for (i, line) in span.text.split('\n').enumerate() {
            if i > 0 {
                out.push_str("<a:br/>");
            }
            if line.is_empty() {
                continue;
            }
            let mut props = String::from(" lang=\"en-US\"");
            if let Some(size) = size {
                let _ = write!(props, " sz=\"{size}\"");
            }
            if span.bold || bold {
                props.push_str(" b=\"1\"");
            }
            if span.italic || italic {
                props.push_str(" i=\"1\"");
            }
            let font = if span.code {
                "<a:latin typeface=\"Courier New\"/>"
            } else {
                ""
            };
            let _ = write!(
                out,
                "<a:r><a:rPr{props} dirty=\"0\">{font}</a:rPr><a:t>{}</a:t></a:r>",
                escape(line)
            );
        }
    }
    out
}

fn placeholder(
    id: usize,
    name: &str,
    ph: &str,
    frame: &str,
    body: &str,
    paragraphs: &str,
) -> String {
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name} {id}\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr>{frame}</p:spPr><p:txBody>{body}<a:lstStyle/>{paragraphs}</p:txBody></p:sp>"
    )
}

fn xfrm(x: i64, y: i64, cx: i64, cy: i64) -> String {
    format!("<a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>")
}

fn table_frame(id: usize, rows: &[Vec<Vec<Span>>], y: i64) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let col_w = WIDTH / cols as i64;
    let mut out = format!(
        "<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"{id}\" name=\"Table {id}\"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp=\"1\"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x=\"{LEFT}\" y=\"{y}\"/><a:ext cx=\"{}\" cy=\"{}\"/></p:xfrm><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/table\"><a:tbl><a:tblPr firstRow=\"1\" bandRow=\"1\"><a:tableStyleId>{TABLE_STYLE}</a:tableStyleId></a:tblPr><a:tblGrid>",
        col_w * cols as i64,
        ROW_H * rows.len() as i64
    );
    for _ in 0..cols {
        let _ = write!(out, "<a:gridCol w=\"{col_w}\"/>");
    }
    out.push_str("</a:tblGrid>");
    for row in rows {
        let _ = write!(out, "<a:tr h=\"{ROW_H}\">");
        for c in 0..cols {
            let cell = row
                .get(c)
                .map(|s| runs(s, false, false, Some(1400)))
                .unwrap_or_default();
            let _ = write!(out, "<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p>{cell}</a:p></a:txBody><a:tcPr/></a:tc>");
        }
        out.push_str("</a:tr>");
    }
    out.push_str("</a:tbl></a:graphicData></a:graphic></p:graphicFrame>");
    out
}

fn master_xml() -> String {
    let mut body_style = String::new();
    for level in 1..=9 {
        let size = [2400, 2000, 1800, 1600, 1600, 1600, 1600, 1600, 1600][level - 1];
        let margin = 228_600 + 457_200 * (level as i64 - 1);
        let bullet = if level % 2 == 1 {
            "\u{2022}"
        } else {
            "\u{2013}"
        };
        let _ = write!(
            body_style,
            "<a:lvl{level}pPr marL=\"{margin}\" indent=\"-228600\" algn=\"l\" defTabSz=\"914400\"><a:lnSpc><a:spcPct val=\"90000\"/></a:lnSpc><a:spcBef><a:spcPts val=\"{}\"/></a:spcBef><a:buFont typeface=\"Arial\"/><a:buChar char=\"{bullet}\"/><a:defRPr sz=\"{size}\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl{level}pPr>",
            if level == 1 { 1000 } else { 500 }
        );
    }
    let title = placeholder(
        2,
        "Title Placeholder",
        "<p:ph type=\"title\"/>",
        &format!("{}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>", xfrm(LEFT, TITLE_Y, WIDTH, 1_325_563)),
        "<a:bodyPr vert=\"horz\" lIns=\"91440\" tIns=\"45720\" rIns=\"91440\" bIns=\"45720\" rtlCol=\"0\" anchor=\"ctr\"><a:normAutofit/></a:bodyPr>",
        "<a:p><a:r><a:rPr lang=\"en-US\"/><a:t>Title</a:t></a:r></a:p>",
    );
    let body = placeholder(
        3,
        "Text Placeholder",
        "<p:ph type=\"body\" idx=\"1\"/>",
        &format!("{}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>", xfrm(LEFT, BODY_Y, WIDTH, 4_351_338)),
        "<a:bodyPr vert=\"horz\" lIns=\"91440\" tIns=\"45720\" rIns=\"91440\" bIns=\"45720\" rtlCol=\"0\"><a:normAutofit/></a:bodyPr>",
        "<a:p><a:pPr lvl=\"0\"/><a:r><a:rPr lang=\"en-US\"/><a:t>Text</a:t></a:r></a:p>",
    );
    format!(
        "{XML_DECL}<p:sldMaster {NS}><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree>{GROUP}{title}{body}</p:spTree></p:cSld>\
<p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst>\
<p:txStyles><p:titleStyle><a:lvl1pPr algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:lnSpc><a:spcPct val=\"90000\"/></a:lnSpc><a:spcBef><a:spcPct val=\"0\"/></a:spcBef><a:buNone/><a:defRPr sz=\"3600\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mj-lt\"/><a:ea typeface=\"+mj-ea\"/><a:cs typeface=\"+mj-cs\"/></a:defRPr></a:lvl1pPr></p:titleStyle>\
<p:bodyStyle>{body_style}</p:bodyStyle><p:otherStyle><a:defPPr><a:defRPr lang=\"en-US\"/></a:defPPr></p:otherStyle></p:txStyles></p:sldMaster>"
    )
}

fn layout_xml() -> String {
    let empty = "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>";
    let title = placeholder(
        2,
        "Title",
        "<p:ph type=\"title\"/>",
        "",
        "<a:bodyPr/>",
        empty,
    );
    let body = placeholder(
        3,
        "Content Placeholder",
        "<p:ph idx=\"1\"/>",
        "",
        "<a:bodyPr/>",
        empty,
    );
    format!(
        "{XML_DECL}<p:sldLayout {NS} type=\"obj\" preserve=\"1\"><p:cSld name=\"Title and Content\"><p:spTree>{GROUP}{title}{body}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"
    )
}

fn rels_xml(relationships: &str) -> String {
    format!(
        "{XML_DECL}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{relationships}</Relationships>"
    )
}

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
/// PowerPoint's built-in "Medium Style 2 - Accent 1".
const TABLE_STYLE: &str = "{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}";
const GROUP: &str = "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>";

const PACKAGE_RELS: &str = "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"ppt/presentation.xml\"/>";
const SLIDE_RELS: &str = "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>";
const MASTER_RELS: &str = "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"../theme/theme1.xml\"/>";
const LAYOUT_RELS: &str = "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/>";

const THEME: &str = "<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Office Theme\"><a:themeElements>\
<a:clrScheme name=\"Office\"><a:dk1><a:sysClr val=\"windowText\" lastClr=\"000000\"/></a:dk1><a:lt1><a:sysClr val=\"window\" lastClr=\"FFFFFF\"/></a:lt1><a:dk2><a:srgbClr val=\"44546A\"/></a:dk2><a:lt2><a:srgbClr val=\"E7E6E6\"/></a:lt2><a:accent1><a:srgbClr val=\"4472C4\"/></a:accent1><a:accent2><a:srgbClr val=\"ED7D31\"/></a:accent2><a:accent3><a:srgbClr val=\"A5A5A5\"/></a:accent3><a:accent4><a:srgbClr val=\"FFC000\"/></a:accent4><a:accent5><a:srgbClr val=\"5B9BD5\"/></a:accent5><a:accent6><a:srgbClr val=\"70AD47\"/></a:accent6><a:hlink><a:srgbClr val=\"0563C1\"/></a:hlink><a:folHlink><a:srgbClr val=\"954F72\"/></a:folHlink></a:clrScheme>\
<a:fontScheme name=\"Office\"><a:majorFont><a:latin typeface=\"Calibri Light\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont><a:minorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme>\
<a:fmtScheme name=\"Office\"><a:fillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:fillStyleLst>\
<a:lnStyleLst><a:ln w=\"6350\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln><a:ln w=\"12700\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln><a:ln w=\"19050\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln></a:lnStyleLst>\
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>\
<a:bgFillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme>\
</a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slides_round_trip() {
        let source = "# Plan\n\n- Ship it\n    - Soon\n\nNotes here.\n\n# Numbers\n\n| Q | Revenue |\n| --- | --- |\n| Q1 | 10 |";
        let bytes = write(&markdown::parse(source)).unwrap();
        let text = read(&bytes).unwrap();
        assert_eq!(text, "# Plan\n\n- Ship it\n    - Soon\n\nNotes here.\n\n# Numbers\n\n| Q | Revenue |\n| --- | --- |\n| Q1 | 10 |");
    }

    #[test]
    fn a_rule_starts_an_untitled_slide_without_doubling_titles() {
        let blocks = markdown::parse("# One\n\ntext\n\n---\n\n# Two\n\nmore");
        let slides = split(&blocks);
        assert_eq!(slides.len(), 2);
        assert!(slides.iter().all(|s| s.title.is_some()));
    }
}
