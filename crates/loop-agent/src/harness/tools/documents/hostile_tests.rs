//! Small hostile files, built here from bytes, against the in-code limits.
//! (The child process that contains whatever these limits miss is tested
//! in `child.rs` and in loop-cli's `document_reader` test.)

use std::io::Write as _;
use std::time::{Duration, Instant};

use super::*;

/// A PDF of `objects` (object `i + 1` is `objects[i]`), with a correct
/// cross-reference table.
pub(super) fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", i + 1).unwrap();
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    out
}

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

/// One page drawing `content` (object 4) with `resources`, then `extra`
/// objects (5 and up).
fn one_page(resources: &str, content: Vec<u8>, extra: Vec<Vec<u8>>) -> Vec<u8> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources {resources} /Contents 4 0 R >>").into_bytes(),
        content,
    ];
    objects.extend(extra);
    pdf(&objects)
}

fn form(resources: Option<&str>, content: &[u8]) -> Vec<u8> {
    let resources = resources
        .map(|r| format!(" /Resources {r}"))
        .unwrap_or_default();
    stream(
        &format!("/Type /XObject /Subtype /Form /BBox [0 0 10 10]{resources}"),
        content,
    )
}

fn pdf_error(bytes: &[u8]) -> String {
    match child::reading(Format::Pdf, bytes) {
        Reading::Refused { error, .. } => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_form_that_draws_itself_is_refused() {
    let own = one_page(
        "<< /XObject << /X 5 0 R >> >>",
        stream("", b"/X Do"),
        vec![form(Some("<< /XObject << /X 5 0 R >> >>"), b"/X Do")],
    );
    assert!(own.len() < 1024);
    assert!(pdf_error(&own).contains("draws itself"));
    // No /Resources: the form draws with the page's, which name it.
    let inherited = one_page(
        "<< /XObject << /X 5 0 R >> >>",
        stream("", b"/X Do"),
        vec![form(None, b"/X Do")],
    );
    assert!(pdf_error(&inherited).contains("draws itself"));
    // Two forms drawing each other.
    let pair = one_page(
        "<< /XObject << /A 5 0 R >> >>",
        stream("", b"/A Do"),
        vec![
            form(Some("<< /XObject << /B 6 0 R >> >>"), b"/B Do"),
            form(Some("<< /XObject << /A 5 0 R >> >>"), b"/A Do"),
        ],
    );
    assert!(pdf_error(&pair).contains("draws itself"));
}

#[test]
fn a_deep_chain_of_forms_is_refused() {
    let depth = 40;
    let extra = (0..depth)
        .map(|i| {
            let next = 5 + i + 1;
            if i + 1 == depth {
                form(Some("<< >>"), b"")
            } else {
                form(
                    Some(&format!("<< /XObject << /N {next} 0 R >> >>")),
                    b"/N Do",
                )
            }
        })
        .collect();
    let bytes = one_page("<< /XObject << /N 5 0 R >> >>", stream("", b"/N Do"), extra);
    assert!(pdf_error(&bytes).contains("deep"));
}

#[test]
fn a_flate_bomb_is_refused_without_inflating_it_whole() {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let chunk = vec![b' '; 1024 * 1024];
    for _ in 0..300 {
        z.write_all(&chunk).unwrap();
    }
    let bomb = z.finish().unwrap();
    assert!(bomb.len() < 1024 * 1024);
    let bytes = one_page("<< >>", stream("/Filter /FlateDecode", &bomb), vec![]);
    let started = Instant::now();
    let error = pdf_error(&bytes);
    assert!(error.contains("decompression bomb"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

pub(super) fn zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in parts {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

const XLSX_BASE: &[(&str, &[u8])] = &[
    ("[Content_Types].xml", br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#),
    ("_rels/.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
    ("xl/workbook.xml", br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
    ("xl/_rels/workbook.xml.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
];

fn xlsx(sheet_data: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{sheet_data}</sheetData></worksheet>"#
    );
    let mut parts = XLSX_BASE.to_vec();
    parts.push(("xl/worksheets/sheet1.xml", sheet.as_bytes()));
    parts.extend_from_slice(extra);
    zip(&parts)
}

fn inline(cell: &str, text: &str) -> String {
    format!(r#"<c r="{cell}" t="inlineStr"><is><t>{text}</t></is></c>"#)
}

#[test]
fn a_sparse_sheet_is_refused_fast() {
    let bytes = xlsx(
        &format!(
            r#"<row r="1">{}</row><row r="1048576">{}</row>"#,
            inline("A1", "a"),
            inline("XFD1048576", "z")
        ),
        &[],
    );
    assert!(bytes.len() < 4096);
    let started = Instant::now();
    let err = sheet::read(&bytes, false).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(err.contains("1048576 rows × 16384 columns"), "{err}");

    // One wide row is allowed, but its text stops at the output budget.
    let bytes = xlsx(
        &format!(
            r#"<row r="1">{}{}</row>"#,
            inline("A1", "a"),
            inline("XFD1", "z")
        ),
        &[],
    );
    let text = sheet::read(&bytes, false).unwrap();
    assert!(text.len() < OUTPUT_BUDGET + 1024, "{}", text.len());
}

#[test]
fn an_ods_that_repeats_cells_is_measured_before_calamine() {
    let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="T"><table:table-row table:number-rows-repeated="1000000"><table:table-cell table:number-columns-repeated="16384" office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let bytes = zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.spreadsheet"),
        ("META-INF/manifest.xml", br#"<?xml version="1.0"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#),
        ("content.xml", content.as_bytes()),
    ]);
    let started = Instant::now();
    let err = sheet::read(&bytes, true).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(err.contains("too many"), "{err}");
}

#[test]
fn parts_that_add_up_past_the_budget_are_refused() {
    // Five 60 MiB parts: each under the per-part limit, 300 MiB together.
    let big = vec![b'0'; 60 * 1024 * 1024];
    let names = ["a.xml", "b.xml", "c.xml", "d.xml", "e.xml"];
    let extra: Vec<(&str, &[u8])> = names.iter().map(|n| (*n, big.as_slice())).collect();
    let bytes = xlsx(
        &format!(r#"<row r="1">{}</row>"#, inline("A1", "x")),
        &extra,
    );
    assert!(bytes.len() < 2 * 1024 * 1024, "{}", bytes.len());
    let err = sheet::read(&bytes, false).unwrap_err();
    assert!(err.contains("zip bomb"), "{err}");

    // The budget is shared by the parts one reader reads.
    let small = zip(&[("a", b"0123456789"), ("b", b"0123456789")]);
    let mut package = xml::Package::with_budget(&small, 15).unwrap();
    assert!(package.text("a").unwrap().is_some());
    assert!(package.text("b").unwrap_err().contains("zip bomb"));
}

fn pptx(slide_ids: &str, slides: &[(&str, String)]) -> Vec<u8> {
    let presentation = format!(
        r#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>{slide_ids}</p:sldIdLst></p:presentation>"#
    );
    let rels = format!(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>"#,
        slides
            .iter()
            .enumerate()
            .map(|(i, (name, _))| format!(
                r#"<Relationship Id="rId{}" Target="slides/{name}"/>"#,
                i + 1
            ))
            .collect::<String>()
    );
    let mut parts: Vec<(String, Vec<u8>)> = vec![
        ("ppt/presentation.xml".into(), presentation.into_bytes()),
        ("ppt/_rels/presentation.xml.rels".into(), rels.into_bytes()),
    ];
    for (name, body) in slides {
        parts.push((format!("ppt/slides/{name}"), body.clone().into_bytes()));
    }
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    zip(&refs)
}

fn slide(title: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
}

#[test]
fn a_slide_listed_many_times_is_read_once() {
    let ids = r#"<p:sldId id="256" r:id="rId1"/>"#.repeat(200_000);
    let bytes = pptx(&ids, &[("slide1.xml", slide("Only"))]);
    assert!(bytes.len() < 1024 * 1024, "{}", bytes.len());
    let started = Instant::now();
    let text = pptx::read(&bytes).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(text, "# Only");
}

#[test]
fn too_many_slides_are_refused() {
    let slides: Vec<(String, String)> = (0..5_001)
        .map(|i| (format!("s{i}.xml"), String::new()))
        .collect();
    let ids: String = (0..5_001)
        .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 1))
        .collect();
    let named: Vec<(&str, String)> = slides
        .iter()
        .map(|(n, b)| (n.as_str(), b.clone()))
        .collect();
    let bytes = pptx(&ids, &named);
    assert!(pptx::read(&bytes).unwrap_err().contains("slides"));
}

#[test]
fn long_documents_stop_at_the_output_budget() {
    let body =
        "<w:p><w:r><w:t>Lorem ipsum dolor sit amet, consectetur adipiscing.</w:t></w:r></w:p>"
            .repeat(200_000);
    let document = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    let bytes = zip(&[("word/document.xml", document.as_bytes())]);
    let converted = match child::reading(Format::Docx, &bytes) {
        Reading::Done(c) => c,
        other => panic!("{other:?}"),
    };
    assert!(converted.truncated);
    assert!(converted.text.len() <= DEFAULT_MAX_BYTES);
}

#[test]
fn oversized_input_is_refused_before_reading() {
    assert!(too_large(MAX_INPUT_BYTES + 1).contains("too large"));
}

#[tokio::test]
async fn ocr_is_off_or_refused_without_any_request() {
    let scanned = || {
        Reading::Done(Converted {
            text: "x".into(),
            truncated: false,
            pdf: Some(PdfInfo {
                readable: false,
                encrypted: false,
            }),
        })
    };
    // OCR off: the little text there is, and how to turn OCR on.
    let (c, by) = pdf::finish(scanned(), b"%PDF", None, None).await.unwrap();
    assert_eq!(by, "reader");
    assert!(c.text.contains(ocr::KEY_ENV), "{}", c.text);
    let err = pdf::finish(
        Reading::Refused {
            error: "bad".into(),
            pdf: None,
        },
        b"%PDF",
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains(ocr::KEY_ENV), "{err}");

    // OCR on, but the PDF is encrypted: nothing is sent. The endpoint is
    // TEST-NET, which never answers, so a request would fail the test by
    // its message.
    let config = ocr::Config {
        key: "k".into(),
        url: "https://192.0.2.1/ocr".into(),
        model: "m".into(),
    };
    let encrypted = Reading::Refused {
        error: "the PDF is password-protected".into(),
        pdf: Some(PdfInfo {
            readable: false,
            encrypted: true,
        }),
    };
    let err = pdf::finish(encrypted, b"%PDF", Some(config.clone()), None)
        .await
        .unwrap_err();
    assert!(err.contains("encrypted, so it is not sent"), "{err}");
    // The reader could not tell, and the bytes name an /Encrypt dictionary.
    let unknown = Reading::Refused {
        error: "bad".into(),
        pdf: None,
    };
    let err = pdf::finish(unknown, b"%PDF << /Encrypt 3 0 R >>", Some(config), None)
        .await
        .unwrap_err();
    assert!(err.contains("encrypted"), "{err}");
}

#[tokio::test]
async fn a_damaged_encrypted_pdf_is_not_sent_to_ocr() {
    // It fails to load, so the reader cannot say it is encrypted.
    let bytes = b"%PDF-1.4\ngarbage\ntrailer\n<< /Root 1 0 R /Encrypt 9 0 R >>\n%%EOF\n";
    let reading = child::reading(Format::Pdf, bytes);
    assert!(matches!(reading, Reading::Refused { .. }), "{reading:?}");
    let config = ocr::Config {
        key: "k".into(),
        url: "https://192.0.2.1/ocr".into(),
        model: "m".into(),
    };
    let err = pdf::finish(reading, bytes, Some(config), None)
        .await
        .unwrap_err();
    assert!(err.contains("encrypted, so it is not sent"), "{err}");
}

// Malformed XML in a part is a read error, never an empty or partial text.

fn refusal(reading: Reading) -> String {
    match reading {
        Reading::Refused { error, .. } => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_docx_with_an_unclosed_document_part_is_refused() {
    // The file the Linux live check made (D1): a valid zip, and a
    // document.xml that stops inside a run.
    let bytes = zip(&[(
        "word/document.xml",
        b"<w:document xmlns:w='x'><w:body><w:p><w:r><w:t>unclosed",
    )]);
    let error = refusal(child::reading(Format::Docx, &bytes));
    assert!(error.contains("malformed XML"), "{error}");
    assert!(error.contains("word/document.xml"), "{error}");
}

#[test]
fn a_docx_cut_off_after_some_paragraphs_is_not_read_as_partial_text() {
    let bytes = zip(&[(
        "word/document.xml",
        br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>kept?</w:t></w:r></w:p><w:p><w:r><w:t>cut"#,
    )]);
    let error = refusal(child::reading(Format::Docx, &bytes));
    assert!(error.contains("malformed XML"), "{error}");
}

#[test]
fn a_docx_with_mismatched_tags_or_no_root_is_refused() {
    for body in [
        &b"<w:document xmlns:w='x'><w:body></w:document>"[..],
        b"<w:document xmlns:w='x'><w:body><w:p></w:body></w:document>",
        b"",
        b"<?xml version=\"1.0\"?>",
        b"not xml at all",
    ] {
        let bytes = zip(&[("word/document.xml", body)]);
        let error = refusal(child::reading(Format::Docx, &bytes));
        assert!(error.contains("malformed XML"), "{body:?}: {error}");
    }
}

#[test]
fn a_docx_with_a_malformed_side_part_is_refused() {
    for part in ["word/styles.xml", "word/numbering.xml"] {
        let bytes = zip(&[
            (
                "word/document.xml",
                br#"<w:document xmlns:w="x"><w:body/></w:document>"#,
            ),
            (part, b"<w:styles xmlns:w='x'><w:style w:styleId='a'>"),
        ]);
        let error = refusal(child::reading(Format::Docx, &bytes));
        assert!(error.contains("malformed XML"), "{part}: {error}");
    }
}

#[test]
fn an_empty_but_valid_docx_reads_as_empty_text() {
    let bytes = zip(&[(
        "word/document.xml",
        br#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body/></w:document>"#,
    )]);
    match child::reading(Format::Docx, &bytes) {
        Reading::Done(c) => {
            assert_eq!(c.text, "");
            assert!(!c.truncated);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_pptx_slide_or_xlsx_sheet_with_malformed_xml_is_refused() {
    let broken =
        r#"<?xml version="1.0"?><p:sld xmlns:p="x" xmlns:a="y"><p:cSld><a:p><a:r><a:t>cut"#;
    let bytes = pptx(
        r#"<p:sldId r:id="rId1"/>"#,
        &[("slide1.xml", broken.to_owned())],
    );
    let error = refusal(child::reading(Format::Pptx, &bytes));
    assert!(error.contains("malformed XML"), "{error}");

    let sheet = br#"<?xml version="1.0"?><worksheet xmlns="x"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>cut"#;
    let mut parts = XLSX_BASE.to_vec();
    parts.push(("xl/worksheets/sheet1.xml", sheet));
    let error = refusal(child::reading(Format::Xlsx, &zip(&parts)));
    assert!(!error.is_empty());
    let read_ok = matches!(
        child::reading(Format::Xlsx, &xlsx("", &[])),
        Reading::Done(_)
    );
    assert!(read_ok, "a valid empty sheet still reads");
}
