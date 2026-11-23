//! Documents for the `read` and `write` tools: PDF, Word (.docx),
//! PowerPoint (.pptx), RTF, Excel (.xlsx) and OpenDocument spreadsheets
//! (.ods).
//!
//! The model only exchanges text. `read` turns a document into markdown, or
//! spreadsheets into the sheet format in [`WRITE_GUIDE`], and `write` builds
//! the document from the same text. The file extension picks the format.

mod child;
mod docx;
mod formula;
mod markdown;
mod ocr;
mod ods;
mod pdf;
mod pdf_guard;
mod pptx;
mod rtf;
mod sheet;
mod xml;

use std::path::{Path, PathBuf};

use serde_json::json;
use tokio_util::sync::CancellationToken;

pub use child::{run_document_reader_child, set_document_reader, DocumentReader, CHILD_ARG};

use crate::harness::types::ExecutionEnv;
use crate::harness::utils::{truncate_head, DEFAULT_MAX_BYTES};
use crate::types::AgentToolResult;
use child::{Converted, Limits, PdfInfo, Reading};
use loop_ai::{TextContent, ToolResultContent};

/// The largest document `read` opens, and the largest previous file a
/// `write` reads back for its diff.
pub(crate) const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;

/// The most text a reader builds before it stops; the result is then cut
/// to [`DEFAULT_MAX_BYTES`] and marked truncated.
pub(super) const OUTPUT_BUDGET: usize = 2 * DEFAULT_MAX_BYTES;

/// The error for a process that has no document reader.
const NO_READER: &str = "this program cannot read documents: no document reader is set up";

/// Added to the tool result and the diff when a write replaces a document.
pub(super) const REBUILT_NOTE: &str =
    "The document was rebuilt from this text: formatting, images, comments, \
and any other content the text does not show were not kept.";

fn too_large(size: u64) -> String {
    format!(
        "the document is too large to read ({} MB; the limit is {} MB)",
        size / (1024 * 1024),
        MAX_INPUT_BYTES / (1024 * 1024)
    )
}

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
pub(super) enum Format {
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
        Self::from_name(&ext)
    }

    /// The format named `name`: its usual file extension.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "pdf" => Format::Pdf,
            "docx" => Format::Docx,
            "pptx" => Format::Pptx,
            "rtf" => Format::Rtf,
            "xlsx" => Format::Xlsx,
            "ods" => Format::Ods,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Docx => "docx",
            Format::Pptx => "pptx",
            Format::Rtf => "rtf",
            Format::Xlsx => "xlsx",
            Format::Ods => "ods",
        }
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
    size: u64,
    cancel: Option<&CancellationToken>,
) -> Option<Result<AgentToolResult, String>> {
    let format = Format::of(path)?;
    let reader = child::current_reader();
    Some(read_document(env, path, size, format, reader, cancel).await)
}

async fn read_document(
    env: &dyn ExecutionEnv,
    path: &Path,
    size: u64,
    format: Format,
    reader: Option<DocumentReader>,
    cancel: Option<&CancellationToken>,
) -> Result<AgentToolResult, String> {
    if size > MAX_INPUT_BYTES {
        return Err(too_large(size));
    }
    let reader = reader.ok_or(NO_READER)?;
    let bytes = env
        .read_binary_file(path)
        .await
        .map_err(|e| e.to_string())?;
    let size = bytes.len();
    if size as u64 > MAX_INPUT_BYTES {
        return Err(too_large(size as u64));
    }
    // The reader-program check compares host paths: no root for a guest env.
    let workspace = match env.path_namespace() {
        loop_workspace::PathNamespace::Host => Some(reader_root(env).await),
        loop_workspace::PathNamespace::Guest => None,
    };
    let (converted, extracted_by) = match format {
        Format::Pdf => {
            let reading = child::convert(
                &reader,
                Limits::default(),
                format,
                bytes.clone(),
                cancel,
                workspace.as_deref(),
            )
            .await?;
            pdf::finish(reading, &bytes, ocr::Config::from_env(), cancel).await?
        }
        _ => match child::convert(
            &reader,
            Limits::default(),
            format,
            bytes,
            cancel,
            workspace.as_deref(),
        )
        .await?
        {
            Reading::Done(converted) => (converted, "reader"),
            Reading::Refused { error, .. } => return Err(error),
        },
    };
    let (text, cut) = truncate_head(&converted.text, DEFAULT_MAX_BYTES);
    let truncated = cut || converted.truncated;
    let text = if truncated {
        format!(
            "{text}\n\n[Truncated: the document has more text than read shows. Do not overwrite it with write: the rest would be lost.]"
        )
    } else {
        text
    };
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
/// or `content` itself for any other file. Only model text is parsed here,
/// so it runs in this process.
pub(super) async fn encode(path: &Path, content: &str) -> Result<Vec<u8>, String> {
    let Some(format) = Format::of(path) else {
        return Ok(content.as_bytes().to_vec());
    };
    let content = content.to_owned();
    blocking(move || from_text(format, &content))
        .await
        .map_err(|e| format!("could not write the {} file: {e}", format.label()))
}

/// Whether `path` is a document `write` builds from text.
pub(super) fn is_document(path: &Path) -> bool {
    Format::of(path).is_some()
}

/// What the file a write replaces says as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Previous {
    Text(String),
    /// The reader could not read it (too large, damaged, refused,
    /// crashed, timed out); the reason.
    Unreadable(String),
    /// It has more text than `read` shows, so the model cannot have seen
    /// all of it.
    TooLong,
}

/// The Workspace root as the reader-program check compares it, resolved
/// through `env` (never a host call on a Workspace path). The check only
/// refuses a reader program that sits inside this root.
///
/// The document read passes no root for a `guest`-namespace env
/// (`FileSystem::path_namespace`). The edit diff (`text_of`) still gets
/// this guest path for a guest env; the check then compares a host
/// program with a guest path and can refuse only when both spell the same.
pub(super) async fn reader_root(env: &dyn ExecutionEnv) -> PathBuf {
    let cwd = env.cwd();
    env.canonical_path(cwd)
        .await
        .unwrap_or_else(|_| cwd.to_path_buf())
}

/// The text to diff a write against: what `bytes`, the file's contents
/// before the write, say as text. A document is read by the document
/// reader, never in this process.
pub(super) async fn text_of(
    path: &Path,
    bytes: &[u8],
    cancel: Option<&CancellationToken>,
    workspace: &Path,
) -> Result<Previous, String> {
    let Some(format) = Format::of(path) else {
        return Ok(Previous::Text(String::from_utf8_lossy(bytes).into_owned()));
    };
    if bytes.is_empty() {
        return Ok(Previous::Text(String::new()));
    }
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Ok(Previous::Unreadable(too_large(bytes.len() as u64)));
    }
    let Some(reader) = child::current_reader() else {
        return Ok(Previous::Unreadable(NO_READER.into()));
    };
    match child::convert(
        &reader,
        Limits::default(),
        format,
        bytes.to_vec(),
        cancel,
        Some(workspace),
    )
    .await
    {
        Ok(Reading::Done(c)) if c.truncated || c.text.len() > DEFAULT_MAX_BYTES => {
            Ok(Previous::TooLong)
        }
        Ok(Reading::Done(c)) => Ok(Previous::Text(c.text)),
        Ok(Reading::Refused { error, .. }) => Ok(Previous::Unreadable(error)),
        Err(e) if cancel.is_some_and(|c| c.is_cancelled()) => Err(e),
        Err(e) => Ok(Previous::Unreadable(e)),
    }
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

/// The document's text without OCR, cut to [`DEFAULT_MAX_BYTES`]. Runs in
/// the reader child.
fn to_text(format: Format, bytes: &[u8]) -> Result<Converted, (String, Option<PdfInfo>)> {
    let (text, pdf) = match format {
        Format::Pdf => {
            let (extracted, info) = pdf::extract(bytes);
            match extracted {
                Ok(text) => (text, Some(info)),
                Err(e) => return Err((e, Some(info))),
            }
        }
        Format::Docx => (docx::read(bytes).map_err(|e| (e, None))?, None),
        Format::Pptx => (pptx::read(bytes).map_err(|e| (e, None))?, None),
        Format::Rtf => (rtf::read(bytes).map_err(|e| (e, None))?, None),
        Format::Xlsx => (sheet::read(bytes, false).map_err(|e| (e, None))?, None),
        Format::Ods => (sheet::read(bytes, true).map_err(|e| (e, None))?, None),
    };
    let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
    Ok(Converted {
        text,
        truncated,
        pdf,
    })
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

/// Run CPU-bound work off the async runtime; a panic becomes an error.
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

#[cfg(test)]
mod hostile_tests;
