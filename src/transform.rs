//! The request transform: turn PDF attachments in an inference request into
//! text before mesh-llm routes it (`request_transform.v1`).
//!
//! - A request with tools (an agent) gets a note with the paths of the
//!   extracted text and the original PDF, plus the full text when it fits
//!   `inline_max_chars`, otherwise a preview.
//! - A request without tools gets the text inline when it fits. A larger one is
//!   rejected with a clear message; querying it (RLM) is not built yet.

use crate::Pdf;
use crate::attachments::{self, Found, Source};
use crate::error::DocError;
use crate::extract::decode_data;
use crate::store::Entry;
use crate::text;
use mesh_llm_plugin::request_transform::{RequestTransformInput, RequestTransformOutput};

pub async fn transform(
    pdf: &Pdf,
    input: RequestTransformInput,
) -> Result<RequestTransformOutput, DocError> {
    let mut body = input.body;
    let found = attachments::find(&body);
    if found.is_empty() {
        return Ok(RequestTransformOutput::unchanged());
    }
    let agent = attachments::is_agent(&body);
    for at in &found {
        let text = attachment_text(pdf, agent, at).await?;
        attachments::replace(&mut body, at, text);
    }
    Ok(RequestTransformOutput::replaced(body))
}

async fn attachment_text(pdf: &Pdf, agent: bool, at: &Found) -> Result<String, DocError> {
    let data = match &at.source {
        Source::Base64(data) => data,
        Source::FileId(id) => {
            return Err(DocError::new(
                400,
                "file_id_unsupported",
                format!(
                    "file id {id:?}: this mesh has no files API, so send the PDF inline \
                     (base64 file_data, or an Anthropic base64 document)"
                ),
            ));
        }
    };
    let name = text::display_name(at.filename.as_deref());
    let bytes = decode_data(data, pdf.settings.max_pdf_bytes())?;
    let (entry, pages, _) = pdf.document(bytes, &name).await?;
    describe(pdf, agent, &entry, &pages, &name)
}

/// What the model sees instead of the PDF.
fn describe(
    pdf: &Pdf,
    agent: bool,
    entry: &Entry,
    pages: &[String],
    name: &str,
) -> Result<String, DocError> {
    let fits = entry.meta.chars <= pdf.settings.inline_max_chars;
    let plural = if entry.meta.pages == 1 { "" } else { "s" };
    if agent {
        let mut out = format!(
            "[Attached PDF \"{name}\", {} page{plural}, decoded to text.\n\
             Extracted text (\"--- Page N ---\" markers): {}\n\
             Original PDF: {}]",
            entry.meta.pages,
            entry.text_path().display(),
            entry.pdf_path().display(),
        );
        if fits {
            out.push_str("\n\n");
            out.push_str(&text::text_with_page_markers(pages));
        } else {
            out.push_str(&format!(
                "\nIt is too long to include here ({} characters). Read or search the extracted \
                 text file with your tools. It begins:\n\n{}",
                entry.meta.chars,
                text::preview(pages)
            ));
        }
        return Ok(out);
    }
    if fits {
        return Ok(format!(
            "[Attached PDF \"{name}\", {} page{plural}]\n\n{}",
            entry.meta.pages,
            text::text_with_page_markers(pages)
        ));
    }
    Err(DocError::too_large(format!(
        "{name} has {} characters of text, over this node's inline limit of {}. \
         Send it from an agent (a request with tools) to get it as a file, or ask the operator \
         to raise inline_max_chars.",
        entry.meta.chars, pdf.settings.inline_max_chars
    )))
}

/// The transform's error as the client will read it.
pub fn plugin_error(error: DocError) -> mesh_llm_plugin::PluginError {
    mesh_llm_plugin::PluginError::invalid_params(format!("documents: {}", error.message))
}
