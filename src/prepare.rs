//! `prepare_documents`: extract every PDF of a request and choose one mode.

use crate::Documents;
use crate::error::DocError;
use crate::settings::PointerPolicy;
use crate::store::{Entry, document_id};
use crate::text;
use crate::tokens::Grant;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

pub const MAX_PDFS: usize = 8;
const DEFAULT_DEADLINE_MS: u64 = 120_000;
const MAX_DEADLINE_MS: u64 = 600_000;
/// Fewer non-space characters per page than this means no usable text layer.
const MIN_CHARS_PER_PAGE: usize = 10;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PrepareArgs {
    pub documents: Vec<StagedDocument>,
    #[serde(default)]
    pub caller: Caller,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StagedDocument {
    pub index: usize,
    /// A file name inside this plugin's staging directory, never a path.
    pub staged: String,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Caller {
    #[serde(default)]
    pub loopback: bool,
    #[serde(default)]
    pub has_tools: bool,
    #[serde(default)]
    pub mode_header: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PrepareAnswer {
    Parts {
        parts: Vec<Part>,
        #[serde(skip_serializing_if = "Option::is_none")]
        route_model: Option<String>,
    },
    Error {
        error: DocError,
    },
}

#[derive(Debug, Serialize)]
pub struct Part {
    pub index: usize,
    pub mode: Mode,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Inline,
    Pointer,
    Rlm,
}

/// One mode for the whole request, from the total extracted text.
pub fn decide_mode(
    total_chars: usize,
    caller: &Caller,
    policy: PointerPolicy,
    inline_max_chars: usize,
) -> Result<Mode, DocError> {
    let header = caller
        .mode_header
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    match header.as_deref() {
        Some("inline") => return Ok(Mode::Inline),
        Some("pointer") if !caller.loopback => {
            return Err(DocError::new(
                400,
                "pointer_unavailable",
                "pointer mode gives local file paths, so it is only for callers on this machine",
            ));
        }
        Some("pointer") if policy == PointerPolicy::Off => {
            return Err(DocError::new(
                400,
                "pointer_unavailable",
                "pointer mode is turned off on this node",
            ));
        }
        Some("pointer") => return Ok(Mode::Pointer),
        Some("rlm") if caller.has_tools => {
            return Err(DocError::new(
                400,
                "rlm_with_tools",
                "rlm mode answers from the document itself and does not take tools",
            ));
        }
        Some("rlm") => return Ok(Mode::Rlm),
        Some(other) => {
            return Err(DocError::new(
                400,
                "invalid_document_mode",
                format!("x-mesh-documents must be inline, pointer or rlm, not {other:?}"),
            ));
        }
        None => {}
    }
    if total_chars <= inline_max_chars {
        return Ok(Mode::Inline);
    }
    if !caller.has_tools {
        return Ok(Mode::Rlm);
    }
    if caller.loopback && policy == PointerPolicy::Auto {
        return Ok(Mode::Pointer);
    }
    let hint = if caller.loopback && policy == PointerPolicy::Header {
        "; send x-mesh-documents: pointer to get local file paths"
    } else {
        ""
    };
    Err(DocError::too_large(format!(
        "the attached PDFs have {total_chars} characters of text, more than the \
         {inline_max_chars} that can be inlined for a request with tools{hint}"
    )))
}

impl Documents {
    pub async fn prepare(&self, args: PrepareArgs) -> PrepareAnswer {
        let deadline_ms = args
            .deadline_ms
            .unwrap_or(DEFAULT_DEADLINE_MS)
            .clamp(1, MAX_DEADLINE_MS);
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        self.maybe_sweep();
        let result = tokio::time::timeout_at(deadline, self.prepare_inner(args, deadline))
            .await
            .unwrap_or_else(|_| Err(DocError::timeout()));
        match result {
            Ok(answer) => answer,
            Err(error) => PrepareAnswer::Error { error },
        }
    }

    async fn prepare_inner(
        &self,
        args: PrepareArgs,
        deadline: Instant,
    ) -> Result<PrepareAnswer, DocError> {
        if args.documents.is_empty() {
            return Err(DocError::new(
                400,
                "no_documents",
                "no documents to prepare",
            ));
        }
        if args.documents.len() > MAX_PDFS {
            return Err(DocError::too_large(format!(
                "at most {MAX_PDFS} PDF attachments per request"
            )));
        }
        // Read every staged file first; after this the host may delete them.
        let mut budget = self.settings.max_total_pdf_bytes();
        let mut staged = Vec::with_capacity(args.documents.len());
        for document in &args.documents {
            let bytes = self.staging.read(&document.staged, budget)?;
            budget -= bytes.len() as u64;
            staged.push(bytes);
        }
        let mut entries = Vec::with_capacity(staged.len());
        for (document, pdf) in args.documents.iter().zip(staged) {
            let name = text::display_name(document.filename.as_deref(), document.index);
            entries.push(self.extract_cached(pdf, &name, deadline).await?);
        }
        let total_chars: usize = entries.iter().map(|entry| entry.meta.chars).sum();
        if total_chars > self.settings.max_total_chars {
            return Err(DocError::too_large(format!(
                "the attached PDFs have {total_chars} characters of text; the limit is {}",
                self.settings.max_total_chars
            )));
        }
        let mode = decide_mode(
            total_chars,
            &args.caller,
            self.settings.pointer,
            self.settings.inline_max_chars,
        )?;
        self.answer(mode, &args, &entries)
    }

    async fn extract_cached(
        &self,
        pdf: Vec<u8>,
        filename: &str,
        deadline: Instant,
    ) -> Result<Entry, DocError> {
        let id = document_id(&pdf);
        if let Some(entry) = self.store.lookup(&id) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(entry);
        }
        let pages = self.tika.extract(pdf.clone(), deadline).await?;
        self.stats.extractions.fetch_add(1, Ordering::Relaxed);
        let text_chars: usize = pages
            .iter()
            .map(|page| page.chars().filter(|c| !c.is_whitespace()).count())
            .sum();
        if text_chars < MIN_CHARS_PER_PAGE * pages.len().max(1) {
            return Err(DocError::new(
                422,
                "pdf_no_text_layer",
                format!(
                    "{filename} has no text layer (it may be scanned); OCR is not done automatically"
                ),
            ));
        }
        self.store.save(&id, &pdf, filename, &pages)
    }

    fn answer(
        &self,
        mode: Mode,
        args: &PrepareArgs,
        entries: &[Entry],
    ) -> Result<PrepareAnswer, DocError> {
        let token = (mode == Mode::Rlm).then(|| {
            self.tokens.mint(Grant {
                documents: entries.iter().map(|entry| entry.id.clone()).collect(),
                caller_model: args.model.clone(),
            })
        });
        let expires = SystemTime::now() + self.store.ttl();
        let mut parts = Vec::with_capacity(entries.len());
        for (document, entry) in args.documents.iter().zip(entries) {
            let text = match mode {
                Mode::Inline => {
                    let pages = self.pages(entry)?;
                    text::inline_part(&entry.meta.filename, &pages)
                }
                Mode::Pointer => {
                    let pages = self.pages(entry)?;
                    text::pointer_part(&text::PointerFacts {
                        filename: &entry.meta.filename,
                        pages: entry.meta.pages,
                        chars: entry.meta.chars,
                        preview: &text::preview(&pages),
                        text_path: &entry.text_path(),
                        pdf_path: &entry.pdf_path(),
                        expires,
                    })
                }
                Mode::Rlm => text::rlm_part(
                    token.as_deref().unwrap_or_default(),
                    document.index,
                    &entry.meta.filename,
                    entry.meta.pages,
                ),
            };
            parts.push(Part {
                index: document.index,
                mode,
                text,
            });
        }
        Ok(PrepareAnswer::Parts {
            parts,
            route_model: token.map(|_| crate::VIRTUAL_MODEL.to_string()),
        })
    }

    fn pages(&self, entry: &Entry) -> Result<Vec<String>, DocError> {
        self.store
            .pages(&entry.id)
            .ok_or_else(|| DocError::internal("stored document disappeared"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(loopback: bool, has_tools: bool, header: Option<&str>) -> Caller {
        Caller {
            loopback,
            has_tools,
            mode_header: header.map(str::to_string),
        }
    }

    #[test]
    fn the_mode_table() {
        let auto = PointerPolicy::Auto;
        let mode = |chars, caller: &Caller, policy| decide_mode(chars, caller, policy, 100);
        assert_eq!(
            mode(100, &caller(false, true, None), auto),
            Ok(Mode::Inline)
        );
        assert_eq!(
            mode(101, &caller(true, true, None), auto),
            Ok(Mode::Pointer)
        );
        assert_eq!(mode(101, &caller(true, false, None), auto), Ok(Mode::Rlm));
        assert_eq!(mode(101, &caller(false, false, None), auto), Ok(Mode::Rlm));
        assert_eq!(
            mode(101, &caller(false, true, None), auto)
                .unwrap_err()
                .status,
            413
        );
        for policy in [PointerPolicy::Header, PointerPolicy::Off] {
            assert_eq!(
                mode(101, &caller(true, true, None), policy)
                    .unwrap_err()
                    .status,
                413
            );
        }
    }

    #[test]
    fn the_header_overrides_size_within_its_rules() {
        let auto = PointerPolicy::Auto;
        let mode = |caller: &Caller, policy| decide_mode(10, caller, policy, 100);
        assert_eq!(
            mode(&caller(true, true, Some("Pointer")), PointerPolicy::Header),
            Ok(Mode::Pointer)
        );
        assert_eq!(
            mode(&caller(false, false, Some("rlm")), auto),
            Ok(Mode::Rlm)
        );
        assert_eq!(
            decide_mode(1_000, &caller(false, true, Some("inline")), auto, 100),
            Ok(Mode::Inline)
        );
        let code = |caller: &Caller, policy| mode(caller, policy).unwrap_err().code;
        assert_eq!(
            code(&caller(false, true, Some("pointer")), auto),
            "pointer_unavailable"
        );
        assert_eq!(
            code(&caller(true, true, Some("pointer")), PointerPolicy::Off),
            "pointer_unavailable"
        );
        assert_eq!(
            code(&caller(true, true, Some("rlm")), auto),
            "rlm_with_tools"
        );
        assert_eq!(
            code(&caller(true, true, Some("ocr")), auto),
            "invalid_document_mode"
        );
    }
}
