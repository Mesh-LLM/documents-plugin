//! Text shaping: Tika XHTML to pages, and the parts the model sees.
//!
//! Extracted text stays user-role data. The delimiters below help the model
//! tell documents apart; they are not an injection boundary.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PAGE_MARKER: &str = "--- Page ";
const PREVIEW_CHARS: usize = 1_500;

/// Split Tika's XHTML (`<div class="page">` per PDF page) into page texts.
pub fn pages_from_xhtml(xhtml: &str) -> Vec<String> {
    let body = xhtml.find("<body").map_or(xhtml, |start| &xhtml[start..]);
    let mut segments = body.split("<div class=\"page\">");
    let before_first_page = segments.next().unwrap_or_default();
    let mut pages: Vec<String> = segments.map(html_to_text).collect();
    let lead = html_to_text(before_first_page);
    if pages.is_empty() {
        pages.push(lead);
    } else if !lead.is_empty() {
        pages[0] = format!("{lead}\n\n{}", pages[0]);
    }
    pages
}

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..open]));
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[open + 1..open + close];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre"
        ) {
            out.push('\n');
        } else if matches!(name.as_str(), "td" | "th") {
            out.push('\t');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(&decode_entities(rest));
    tidy(&out)
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let candidate = &rest[amp..];
        let decoded = candidate
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| {
                let entity = &candidate[1..end];
                let ch = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                        .and_then(char::from_u32),
                };
                ch.map(|ch| (ch, end))
            });
        match decoded {
            Some((ch, end)) => {
                out.push(ch);
                rest = &candidate[end + 1..];
            }
            None => {
                out.push('&');
                rest = &candidate[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Trim lines and collapse runs of blank lines.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

/// The whole document as markdown-ish text with page markers.
pub fn text_with_page_markers(pages: &[String]) -> String {
    let mut out = String::new();
    for (index, page) in pages.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("{PAGE_MARKER}{} ---\n\n{page}", index + 1));
    }
    out
}

pub fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Keep a document's own text from closing our delimiter.
fn escape_body(text: &str) -> String {
    text.replace("</document", "<\\/document")
}

pub fn display_name(filename: Option<&str>, index: usize) -> String {
    filename
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(200).collect())
        .unwrap_or_else(|| format!("document-{}.pdf", index + 1))
}

pub fn inline_part(filename: &str, pages: &[String]) -> String {
    format!(
        "<document filename=\"{}\" pages=\"{}\">\n{}\n</document>",
        escape_attr(filename),
        pages.len(),
        escape_body(&text_with_page_markers(pages))
    )
}

pub struct PointerFacts<'a> {
    pub filename: &'a str,
    pub pages: usize,
    pub chars: usize,
    pub preview: &'a str,
    pub text_path: &'a std::path::Path,
    pub pdf_path: &'a std::path::Path,
    pub expires: SystemTime,
}

pub fn pointer_part(facts: &PointerFacts<'_>) -> String {
    format!(
        "<document filename=\"{name}\" pages=\"{pages}\" chars=\"{chars}\" mode=\"pointer\">\n\
         This PDF is too large to include here. Its extracted text and the original are \
         files on this machine; read them with your tools.\n\
         - Extracted text: {text} (each page starts with a \"{PAGE_MARKER}N ---\" line)\n\
         - Original PDF: {pdf}\n\
         These files are kept until at least {expires} (24 hours after their last use).\n\n\
         Beginning of the text:\n{preview}\n</document>",
        name = escape_attr(facts.filename),
        pages = facts.pages,
        chars = facts.chars,
        text = facts.text_path.display(),
        pdf = facts.pdf_path.display(),
        expires = utc_timestamp(facts.expires),
        preview = escape_body(facts.preview),
    )
}

pub fn rlm_part(token: &str, index: usize, filename: &str, pages: usize) -> String {
    format!(
        "<mesh-document token=\"{token}\" index=\"{index}\" filename=\"{}\" pages=\"{pages}\"/>",
        escape_attr(filename)
    )
}

pub fn preview(pages: &[String]) -> String {
    let text = text_with_page_markers(pages);
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// `YYYY-MM-DD HH:MM UTC` without a date crate.
pub fn utc_timestamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xhtml_pages_become_clean_text() {
        let xhtml = "<html><head><title>x</title></head><body>\
            <div class=\"page\"><p>Hello &amp; welcome</p><p>line&#32;two</p></div>\
            <div class=\"page\"><p>Second &lt;page&gt;</p></div></body></html>";
        assert_eq!(
            pages_from_xhtml(xhtml),
            vec!["Hello & welcome\n\nline two", "Second <page>"]
        );
    }

    #[test]
    fn text_without_pages_is_one_page() {
        assert_eq!(pages_from_xhtml("<body><p>only</p></body>"), vec!["only"]);
    }

    #[test]
    fn parts_escape_names_and_closing_tags() {
        let part = inline_part("a\"b<.pdf", &["x </document> y".to_string()]);
        assert!(part.starts_with("<document filename=\"a&quot;b&lt;.pdf\" pages=\"1\">"));
        assert!(part.contains("x <\\/document> y"));
        assert!(part.contains("--- Page 1 ---"));
    }

    #[test]
    fn timestamps_are_utc_dates() {
        let time = UNIX_EPOCH + Duration::from_secs(1_794_362_773);
        assert_eq!(utc_timestamp(time), "2026-11-11 02:06 UTC");
    }
}
