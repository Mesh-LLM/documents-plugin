//! Finding PDF attachments in a request and putting text in their place.
//!
//! The host runs request transforms on the Chat Completions shape: it has
//! already converted `/v1/responses` and `/v1/messages` bodies. Attachments
//! arrive as one of:
//! - Chat: `{"type":"file","file":{"file_data":"data:application/pdf;base64,…","filename":…}}`
//!   or `{"type":"file","file":{"file_id":"file-…"}}`
//! - From Responses `input_file` and Anthropic `document` blocks:
//!   `{"type":"input_file","input_file":{"file_data"|"file_id"|"file_url",…}}`
//!
//! Parts are found in every message, including `tool` messages (Claude Code's
//! Read tool returns a PDF as a document inside a tool result).

use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Base64 (a `data:` URL prefix is allowed).
    Base64(String),
    /// A provider file id: there is no PDF here to read.
    FileId(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// (message index, content part index).
    pub message: usize,
    pub part: usize,
    pub source: Source,
    pub filename: Option<String>,
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn is_pdf_data(data: &str, declared_pdf: bool) -> bool {
    match data.strip_prefix("data:") {
        Some(rest) => rest
            .split([';', ','])
            .next()
            .is_some_and(|mime| mime.eq_ignore_ascii_case("application/pdf")),
        // Plain base64: trust a declared PDF type, else sniff "%PDF-" (JVBERi0).
        None => declared_pdf || data.trim_start().starts_with("JVBERi0"),
    }
}

/// A file container: `file_data` / `file_id` / `filename` / `mime_type`.
fn pdf_in_container(container: &Value) -> Option<(Source, Option<String>)> {
    let filename = str_at(container, "filename")
        .or_else(|| str_at(container, "file_name"))
        .map(str::to_string);
    let declared_pdf = filename
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".pdf"))
        || str_at(container, "mime_type") == Some("application/pdf");
    if let Some(data) = str_at(container, "file_data").or_else(|| str_at(container, "url")) {
        return is_pdf_data(data, declared_pdf)
            .then(|| (Source::Base64(data.to_string()), filename));
    }
    str_at(container, "file_id").map(|id| (Source::FileId(id.to_string()), filename))
}

fn pdf_in_part(part: &Value) -> Option<(Source, Option<String>)> {
    let key = match str_at(part, "type")? {
        "file" => "file",
        "input_file" => "input_file",
        _ => return None,
    };
    pdf_in_container(part.get(key)?)
}

/// Every PDF attachment in a Chat Completions request, in order.
pub fn find(body: &Value) -> Vec<Found> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (message, value) in messages.iter().enumerate() {
        let Some(parts) = value.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (part, value) in parts.iter().enumerate() {
            if let Some((source, filename)) = pdf_in_part(value) {
                found.push(Found {
                    message,
                    part,
                    source,
                    filename,
                });
            }
        }
    }
    found
}

/// Put a text part where the found part was.
pub fn replace(body: &mut Value, at: &Found, text: String) {
    if let Some(part) = body
        .get_mut("messages")
        .and_then(|messages| messages.get_mut(at.message))
        .and_then(|message| message.get_mut("content"))
        .and_then(|content| content.get_mut(at.part))
    {
        *part = json!({"type": "text", "text": text});
    }
}

/// An agent is a caller that gave the model tools.
pub fn is_agent(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PDF_URL: &str = "data:application/pdf;base64,JVBERi0xLjc=";

    #[test]
    fn chat_file_parts() {
        let body = json!({"messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": [
                {"type": "text", "text": "summarize"},
                {"type": "file", "file": {"filename": "a.pdf", "file_data": PDF_URL}},
                {"type": "file", "file": {"file_id": "file-abc"}},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,xx"}},
                {"type": "file", "file": {"filename": "a.txt", "file_data": "data:text/plain;base64,aGk="}}
            ]}
        ]});
        let found = find(&body);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].message, found[0].part), (1, 1));
        assert_eq!(found[0].source, Source::Base64(PDF_URL.into()));
        assert_eq!(found[0].filename.as_deref(), Some("a.pdf"));
        assert_eq!(found[1].source, Source::FileId("file-abc".into()));
    }

    #[test]
    fn normalized_input_files_including_tool_messages() {
        // What the host makes of Responses input_file and Anthropic document
        // blocks (the latter also inside a tool_result).
        let body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "input_file", "input_file": {"filename": "r.pdf", "file_data": PDF_URL}},
                {"type": "input_file", "input_file": {"file_data": "JVBERi0xLjc=", "mime_type": "application/pdf"}},
                {"type": "input_file", "input_file": {"file_url": "https://example.com/x.pdf"}}
            ]},
            {"role": "tool", "tool_call_id": "t1", "content": [
                {"type": "input_file", "input_file": {"file_data": PDF_URL}}
            ]}
        ]});
        let found = find(&body);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].filename.as_deref(), Some("r.pdf"));
        assert_eq!(found[1].source, Source::Base64("JVBERi0xLjc=".into()));
        assert_eq!((found[2].message, found[2].part), (1, 0));
    }

    #[test]
    fn replacing_keeps_order_and_neighbours() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "before"},
            {"type": "file", "file": {"file_data": PDF_URL}},
            {"type": "text", "text": "after"}
        ]}]});
        let found = find(&body);
        replace(&mut body, &found[0], "PDF TEXT".into());
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "before"},
                {"type": "text", "text": "PDF TEXT"},
                {"type": "text", "text": "after"}
            ])
        );
    }

    #[test]
    fn agents_have_tools() {
        assert!(is_agent(&json!({"tools": [{"type": "function"}]})));
        assert!(!is_agent(&json!({"tools": []})));
        assert!(!is_agent(&json!({})));
    }
}
