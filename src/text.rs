//! Text shaping: Tika XHTML to pages, page markers and previews.

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

pub fn display_name(filename: Option<&str>) -> String {
    filename
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(200).collect())
        .unwrap_or_else(|| "document.pdf".into())
}

pub fn preview(pages: &[String]) -> String {
    let text = text_with_page_markers(pages);
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
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
}
