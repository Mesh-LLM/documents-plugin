//! The `extract` tool: one PDF in, its text (or a file path to it) out.

use crate::Pdf;
use crate::error::DocError;
use crate::store::{Entry, document_id};
use crate::text;
use base64::Engine;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tokio::time::Instant;

/// Fewer non-space characters per page than this means no usable text.
const MIN_CHARS_PER_PAGE: usize = 10;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractArgs {
    /// Absolute path of a PDF on this machine (agents). Give this or `data_base64`.
    #[serde(default)]
    pub path: Option<String>,
    /// The PDF itself, base64 (a `data:application/pdf;base64,` prefix is fine).
    #[serde(default)]
    pub data_base64: Option<String>,
    /// Name to report for `data_base64` input.
    #[serde(default)]
    pub filename: Option<String>,
    /// `auto` (default): text if it fits `inline_max_chars`, else a file path.
    /// `text`: always return the text. `file`: always return the file path.
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Extracted {
    pub filename: String,
    pub pages: usize,
    pub chars: usize,
    /// True when it came from the cache rather than a new Tika run.
    pub cached: bool,
    /// `text` or `file`.
    pub output: &'static str,
    /// The whole text with `--- Page N ---` markers (`output: text`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Extracted text file with page markers (`output: file`). Read or grep it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_path: Option<PathBuf>,
    /// A copy of the original PDF next to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_path: Option<PathBuf>,
    /// The first ~1,500 characters (`output: file`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    Auto,
    Text,
    File,
}

fn parse_output(value: Option<&str>) -> Result<Output, DocError> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("auto") => Ok(Output::Auto),
        Some("text") => Ok(Output::Text),
        Some("file") => Ok(Output::File),
        Some(other) => Err(DocError::new(
            400,
            "invalid_output",
            format!("output must be auto, text or file, not {other:?}"),
        )),
    }
}

fn read_path(path: &str, max_bytes: u64) -> Result<(Vec<u8>, String), DocError> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        return Err(DocError::new(400, "invalid_path", "path must be absolute"));
    }
    let not_found = |e: std::io::Error| {
        DocError::new(404, "file_not_found", format!("{}: {e}", path.display()))
    };
    let metadata = std::fs::metadata(path).map_err(not_found)?;
    if !metadata.is_file() {
        return Err(DocError::new(400, "invalid_path", "path is not a file"));
    }
    if metadata.len() > max_bytes {
        return Err(too_big(metadata.len(), max_bytes));
    }
    let bytes = std::fs::read(path).map_err(not_found)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    Ok((bytes, text::display_name(name.as_deref())))
}

pub(crate) fn decode_data(data: &str, max_bytes: u64) -> Result<Vec<u8>, DocError> {
    let data = data.trim();
    let data = match data.strip_prefix("data:") {
        Some(rest) => rest.split_once(',').map_or("", |(_, body)| body),
        None => data,
    };
    // Base64 is 4/3 the size; refuse before decoding something huge.
    if data.len() as u64 / 4 * 3 > max_bytes + 3 {
        return Err(too_big(data.len() as u64 / 4 * 3, max_bytes));
    }
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| DocError::new(400, "invalid_base64", format!("data_base64: {e}")))
}

fn too_big(bytes: u64, max: u64) -> DocError {
    DocError::too_large(format!(
        "the PDF is {} MB; the limit is {} MB",
        bytes.div_ceil(1024 * 1024),
        max / (1024 * 1024)
    ))
}

impl Pdf {
    pub async fn extract(&self, args: ExtractArgs) -> Result<Extracted, DocError> {
        let output = parse_output(args.output.as_deref())?;
        let max = self.settings.max_pdf_bytes();
        let (pdf, filename) = match (&args.path, &args.data_base64) {
            (Some(path), None) => read_path(path, max)?,
            (None, Some(data)) => (
                decode_data(data, max)?,
                text::display_name(args.filename.as_deref()),
            ),
            _ => {
                return Err(DocError::new(
                    400,
                    "invalid_arguments",
                    "give exactly one of path or data_base64",
                ));
            }
        };
        let (entry, pages, cached) = self.document(pdf, &filename).await?;
        let wants_text = match output {
            Output::Text => true,
            Output::File => false,
            Output::Auto => entry.meta.chars <= self.settings.inline_max_chars,
        };
        Ok(Extracted {
            filename,
            pages: entry.meta.pages,
            chars: entry.meta.chars,
            cached,
            output: if wants_text { "text" } else { "file" },
            text: wants_text.then(|| text::text_with_page_markers(&pages)),
            text_path: (!wants_text).then(|| entry.text_path()),
            pdf_path: (!wants_text).then(|| entry.pdf_path()),
            preview: (!wants_text).then(|| text::preview(&pages)),
        })
    }

    /// A PDF's stored extraction and its pages, extracting it first if new.
    pub(crate) async fn document(
        &self,
        pdf: Vec<u8>,
        filename: &str,
    ) -> Result<(Entry, Vec<String>, bool), DocError> {
        if pdf.len() as u64 > self.settings.max_pdf_bytes() {
            return Err(too_big(pdf.len() as u64, self.settings.max_pdf_bytes()));
        }
        if !pdf.starts_with(b"%PDF-") {
            return Err(DocError::new(
                400,
                "not_a_pdf",
                format!("{filename} is not a PDF"),
            ));
        }
        self.maybe_sweep();
        let (entry, cached) = self.extract_cached(pdf, filename).await?;
        let pages = self.stored_pages(&entry)?;
        Ok((entry, pages, cached))
    }

    pub(crate) fn stored_pages(&self, entry: &Entry) -> Result<Vec<String>, DocError> {
        self.store
            .pages(&entry.id)
            .ok_or_else(|| DocError::internal("stored document disappeared"))
    }

    async fn extract_cached(
        &self,
        pdf: Vec<u8>,
        filename: &str,
    ) -> Result<(Entry, bool), DocError> {
        let id = document_id(&pdf);
        if let Some(entry) = self.store.lookup(&id) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok((entry, true));
        }
        let deadline = Instant::now() + self.settings.timeout();
        let pages = self.tika.extract(pdf.clone(), deadline).await?;
        self.stats.extractions.fetch_add(1, Ordering::Relaxed);
        let text_chars: usize = pages
            .iter()
            .map(|page| page.chars().filter(|c| !c.is_whitespace()).count())
            .sum();
        if text_chars < MIN_CHARS_PER_PAGE * pages.len().max(1) {
            let why = if self.settings.ocr {
                "(blank pages, or OCR could not read them; OCR needs tesseract on PATH or in tesseract_dir)"
            } else {
                "(OCR is off)"
            };
            return Err(DocError::new(
                422,
                "pdf_no_text",
                format!("no text found in {filename} {why}"),
            ));
        }
        Ok((self.store.save(&id, &pdf, filename, &pages)?, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_modes_parse() {
        assert_eq!(parse_output(None), Ok(Output::Auto));
        assert_eq!(parse_output(Some("FILE")), Ok(Output::File));
        assert_eq!(parse_output(Some("text")), Ok(Output::Text));
        assert_eq!(
            parse_output(Some("pdf")).unwrap_err().code,
            "invalid_output"
        );
    }

    #[test]
    fn base64_with_or_without_a_data_url() {
        let plain = "JVBERi0xLjc=";
        assert_eq!(decode_data(plain, 1024).unwrap(), b"%PDF-1.7");
        let url = format!("data:application/pdf;base64,{plain}");
        assert_eq!(decode_data(&url, 1024).unwrap(), b"%PDF-1.7");
        assert_eq!(decode_data("!!", 1024).unwrap_err().code, "invalid_base64");
        assert_eq!(
            decode_data(&"A".repeat(4000), 1024).unwrap_err().status,
            413
        );
    }

    #[test]
    fn paths_must_be_absolute_files() {
        assert_eq!(read_path("rel.pdf", 1024).unwrap_err().code, "invalid_path");
        assert_eq!(read_path("/", 1024).unwrap_err().code, "invalid_path");
        assert_eq!(
            read_path("/nonexistent/x.pdf", 1024).unwrap_err().code,
            "file_not_found"
        );
    }
}
