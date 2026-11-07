//! Zip packages and a flat XML event list for the Office and OpenDocument formats.

use std::io::{Cursor, Read, Write};

use quick_xml::events::{BytesStart, Event};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// The most one part of a package may inflate to, against zip bombs.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// A zip package being read.
pub(super) struct Package<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
}

impl<'a> Package<'a> {
    pub(super) fn open(bytes: &'a [u8]) -> Result<Self, String> {
        let zip = ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("not a valid Office file (zip): {e}"))?;
        Ok(Self { zip })
    }

    /// The UTF-8 text of part `name`, or `None` when the package has no such part.
    pub(super) fn text(&mut self, name: &str) -> Result<Option<String>, String> {
        let file = match self.zip.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => return Err(format!("{name}: {e}")),
        };
        let mut text = String::new();
        file.take(MAX_PART_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("{name}: {e}"))?;
        if text.len() as u64 > MAX_PART_BYTES {
            return Err(format!("{name} is too large"));
        }
        Ok(Some(text))
    }
}

/// A zip package of `parts`; a `mimetype` part is stored uncompressed, as
/// OpenDocument requires it first and unpacked.
pub(super) fn package(parts: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in parts {
        let method = if *name == "mimetype" {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(method),
        )
        .map_err(|e| e.to_string())?;
        zip.write_all(contents.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

/// One XML event; names are local (no namespace prefix). An empty element
/// is an `Open` followed by its `Close`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Node {
    Open {
        name: String,
        /// Attribute names keep their prefix (`r:id`); see [`attr`].
        attrs: Vec<(String, String)>,
    },
    Close(String),
    Text(String),
}

/// The events of `xml`, with entities and character references resolved.
pub(super) fn nodes(xml: &str) -> Result<Vec<Node>, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("malformed XML: {e}"))?;
        match event {
            Event::Start(e) => out.push(open(&e)?),
            Event::Empty(e) => {
                let node = open(&e)?;
                let name = match &node {
                    Node::Open { name, .. } => name.clone(),
                    _ => unreachable!(),
                };
                out.push(node);
                out.push(Node::Close(name));
            }
            Event::End(e) => out.push(Node::Close(local(e.local_name().as_ref()))),
            Event::Text(e) => {
                let text = e.xml10_content().map_err(|e| e.to_string())?;
                out.push(Node::Text(text.into_owned()));
            }
            Event::CData(e) => {
                let text = e.decode().map_err(|e| e.to_string())?;
                out.push(Node::Text(text.into_owned()));
            }
            Event::GeneralRef(e) => {
                let ch = match e.resolve_char_ref() {
                    Ok(Some(ch)) => Some(ch),
                    _ => match e.decode().map_err(|e| e.to_string())?.as_ref() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    },
                };
                if let Some(ch) = ch {
                    out.push(Node::Text(ch.to_string()));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn open(e: &BytesStart) -> Result<Node, String> {
    let mut attrs = Vec::new();
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|e| e.to_string())?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?;
        attrs.push((key, value.into_owned()));
    }
    Ok(Node::Open {
        name: local(e.local_name().as_ref()),
        attrs,
    })
}

fn local(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// Attribute `name` exactly (`r:id`), else by local name (`val` finds `w:val`).
pub(super) fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .or_else(|| {
            attrs
                .iter()
                .find(|(k, _)| k.rsplit(':').next() == Some(name))
        })
        .map(|(_, v)| v.as_str())
}

/// `text` escaped for XML content and attribute values, without the
/// control characters XML 1.0 forbids.
pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The part a relationship `target` names, relative to the folder `base`
/// (`ppt/`), or absolute when it starts with `/`.
pub(super) fn resolve(base: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_owned();
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for piece in target.split('/') {
        match piece {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Relationship id to target from a `.rels` part.
pub(super) fn relationships(
    xml: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut out = std::collections::HashMap::new();
    for node in nodes(xml)? {
        if let Node::Open { name, attrs } = node {
            if name == "Relationship" {
                if let (Some(id), Some(target)) = (attr(&attrs, "Id"), attr(&attrs, "Target")) {
                    out.insert(id.to_owned(), target.to_owned());
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_resolve_entities_and_split_empty_elements() {
        let got = nodes(r#"<w:p><w:t>a &amp; b&#33;</w:t><w:br/></w:p>"#).unwrap();
        let text: String = got
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "a & b!");
        assert!(got.contains(&Node::Close("br".into())));
    }

    #[test]
    fn attr_prefers_the_exact_name() {
        let attrs = vec![
            ("id".to_owned(), "256".to_owned()),
            ("r:id".to_owned(), "rId2".to_owned()),
        ];
        assert_eq!(attr(&attrs, "r:id"), Some("rId2"));
        assert_eq!(attr(&attrs, "id"), Some("256"));
    }

    #[test]
    fn resolve_targets() {
        assert_eq!(
            resolve("ppt/", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        assert_eq!(
            resolve("ppt/slides/", "../slideLayouts/a.xml"),
            "ppt/slideLayouts/a.xml"
        );
        assert_eq!(resolve("ppt/", "/ppt/slides/s.xml"), "ppt/slides/s.xml");
    }
}
