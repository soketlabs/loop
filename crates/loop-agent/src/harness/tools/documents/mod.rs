//! Documents for the `read` and `write` tools: PDF, Word (.docx),
//! PowerPoint (.pptx), RTF, Excel (.xlsx) and OpenDocument spreadsheets
//! (.ods).
//!
//! The model only exchanges text. `read` turns a document into markdown, or
//! spreadsheets into the sheet format in [`WRITE_GUIDE`], and `write` builds
//! the document from the same text. The file extension picks the format.

mod docx;
mod formula;
mod markdown;
mod ocr;
mod ods;
mod pdf;
mod pptx;
mod rtf;
mod sheet;
mod xml;

use std::path::Path;

use serde_json::json;

use crate::harness::types::ExecutionEnv;
use crate::harness::utils::{truncate_head, DEFAULT_MAX_BYTES};
use crate::types::AgentToolResult;
use loop_ai::{TextContent, ToolResultContent};

/// Added to the `read` tool's description.
pub(super) const READ_GUIDE: &str = " PDF, Word (.docx), PowerPoint (.pptx) and RTF files are returned as \
markdown (a PowerPoint deck as one `# Title` section per slide); Excel (.xlsx) and OpenDocument (.ods) \
spreadsheets in the sheet format the write tool describes, with formulas.";

/// Added to the `write` tool's description.
pub(super) const WRITE_GUIDE: &str = r#" The file extension picks the format, and `content` is always text:
- .docx, .rtf, .pdf: markdown. Headings (# to ######), paragraphs, **bold**, *italic*, `code`, bullet and numbered lists (nest by indenting 4 spaces), pipe tables, fenced code blocks, > quotes, and --- rules. PDFs use the standard fonts, so only Latin text renders.
- .pptx: markdown where each top-level heading (`# Title`) starts a slide and is its title (--- starts an untitled slide). A slide holds bullet or numbered lists, paragraphs, `##` subheadings (bold lines), code, and pipe tables.
- .xlsx, .ods: one block per sheet, exactly like this:
## Sheet: Budget
| | A | B | C |
|---|---|---|---|
| 1 | **Item** | **Cost** | **Share** |
| 2 | Rent | 1200 | =B2/B$4 |
| 3 | Food | 450.5 | =B3/B$4 |
| 4 | **Total** | =SUM(B2:B3) | 100% |
  The header row is an empty cell followed by column letters (gaps allowed, e.g. A, B, E); every other row starts with its row number (gaps allowed). Cell text sets the type: `=...` is a formula; 42, -3.5, 1.2e6 are numbers (no thousands separators or currency symbols); 12.5% is a percentage; TRUE/FALSE are booleans; 2024-01-31 or 2024-01-31 14:30 are dates; anything else is text. Start a cell with ' to keep it text ('00123, '=not a formula). Wrap a whole cell in **...** to make it bold. Write \| for a literal pipe and <br> for a line break inside a cell; leave a cell empty for none.
  Formulas use English Excel syntax in both formats: comma-separated arguments, A1 references ($A$1 absolute, B2:B10 ranges, A:A whole columns), other sheets as Sheet2!A1 or 'My Sheet'!A1:B5, text in double quotes. Write formulas, not their results: spreadsheet apps calculate them when the file opens. Lines outside the sheet tables are ignored."#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Pdf,
    Docx,
    Pptx,
    Rtf,
    Xlsx,
    Ods,
}

impl Format {
    fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "pdf" => Format::Pdf,
            "docx" => Format::Docx,
            "pptx" => Format::Pptx,
            "rtf" => Format::Rtf,
            "xlsx" => Format::Xlsx,
            "ods" => Format::Ods,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Format::Pdf => "PDF",
            Format::Docx => "Word",
            Format::Pptx => "PowerPoint",
            Format::Rtf => "RTF",
            Format::Xlsx => "Excel",
            Format::Ods => "OpenDocument spreadsheet",
        }
    }
}

/// The `read` result for a document, or `None` when `path` is not one.
pub(super) async fn read(
    env: &dyn ExecutionEnv,
    path: &Path,
) -> Option<Result<AgentToolResult, String>> {
    let format = Format::of(path)?;
    Some(read_document(env, path, format).await)
}

async fn read_document(
    env: &dyn ExecutionEnv,
    path: &Path,
    format: Format,
) -> Result<AgentToolResult, String> {
    let bytes = env
        .read_binary_file(path)
        .await
        .map_err(|e| e.to_string())?;
    let size = bytes.len();
    let (text, extracted_by) = match format {
        Format::Pdf => pdf::read(bytes).await?,
        _ => (blocking(move || to_text(format, &bytes)).await?, "reader"),
    };
    let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text,
            text_signature: None,
        })],
        details: json!({
            "path": path,
            "format": format.label(),
            "bytes": size,
            "truncated": truncated,
            "extractedBy": extracted_by,
        }),
        usage: None,
        added_tool_names: None,
        terminate: None,
    })
}

/// The bytes `write` stores for `content` at `path`: the built document,
/// or `content` itself for any other file.
pub(super) async fn encode(path: &Path, content: &str) -> Result<Vec<u8>, String> {
    let Some(format) = Format::of(path) else {
        return Ok(content.as_bytes().to_vec());
    };
    let content = content.to_owned();
    blocking(move || from_text(format, &content))
        .await
        .map_err(|e| format!("could not write the {} file: {e}", format.label()))
}

/// What `path` holds before a write, or `None` when it does not exist: the
/// bytes to restore on revert, and the text to diff the new content against.
pub(super) async fn previous(env: &dyn ExecutionEnv, path: &Path) -> Option<(Vec<u8>, String)> {
    let Some(format) = Format::of(path) else {
        return match env.read_text_file(path).await {
            Ok(text) => Some((text.clone().into_bytes(), text)),
            Err(_) => env
                .read_binary_file(path)
                .await
                .ok()
                .map(|b| (b, String::new())),
        };
    };
    let bytes = env.read_binary_file(path).await.ok()?;
    let copy = bytes.clone();
    let text = blocking(move || to_text(format, &copy))
        .await
        .unwrap_or_default();
    Some((bytes, text))
}

/// `edit` replaces text in place, which a document has no form for.
pub(super) fn check_editable(path: &Path) -> Result<(), String> {
    match Format::of(path) {
        Some(format) => Err(format!(
            "{} files can't be edited in place: read the file, then write the whole document with your changes",
            format.label()
        )),
        None => Ok(()),
    }
}

/// The document's text without OCR.
fn to_text(format: Format, bytes: &[u8]) -> Result<String, String> {
    match format {
        Format::Pdf => pdf::extract(bytes).map(|p| p.markdown),
        Format::Docx => docx::read(bytes),
        Format::Pptx => pptx::read(bytes),
        Format::Rtf => rtf::read(bytes),
        Format::Xlsx => sheet::read(bytes, false),
        Format::Ods => sheet::read(bytes, true),
    }
}

fn from_text(format: Format, text: &str) -> Result<Vec<u8>, String> {
    match format {
        Format::Pdf => Ok(pdf::write(&markdown::parse(text))),
        Format::Docx => docx::write(&markdown::parse(text)),
        Format::Pptx => pptx::write(&markdown::parse(text)),
        Format::Rtf => Ok(rtf::write(&markdown::parse(text)).into_bytes()),
        Format::Xlsx => sheet::write_xlsx(&sheet::parse(text)?),
        Format::Ods => ods::write(&sheet::parse(text)?),
    }
}

/// Run CPU-bound parsing off the async runtime; a parser panic on a damaged
/// file becomes an error.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        if e.is_panic() {
            "the file is damaged or uses features the reader does not support".to_owned()
        } else {
            e.to_string()
        }
    })?
}
