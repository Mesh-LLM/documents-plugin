//! The pages of a turn's documents, numbered 1.. across all of them, and the
//! deterministic `search` and `read` tools over them.

use crate::text::PAGE_MARKER;
use std::collections::{HashMap, HashSet};

const SEARCH_HITS: usize = 8;
const SNIPPET_CHARS: usize = 320;
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "with", "what", "which", "who", "how", "does",
    "this", "that", "from", "into", "about", "is", "of", "to", "in", "on", "a", "an", "it", "be",
    "by", "as", "at", "or", "do", "did", "can", "page", "document",
];

pub struct DocumentInfo {
    pub filename: String,
    pub first_page: usize,
    pub pages: usize,
    pub chars: usize,
}

pub struct Corpus {
    pub documents: Vec<DocumentInfo>,
    pages: Vec<String>,
    lowered: Vec<String>,
}

pub fn truncate_chars(text: &str, max_chars: usize, note: &str) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => {
            let keep = max_chars.saturating_sub(note.chars().count() + 2);
            let cut = text.char_indices().nth(keep).map_or(cut, |(i, _)| i);
            format!("{}\n{note}", &text[..cut])
        }
        None => text.to_string(),
    }
}

/// Cut `text` to at most `max_bytes`, on a character boundary.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

impl Corpus {
    pub fn new(documents: Vec<(String, Vec<String>)>) -> Self {
        let mut infos = Vec::new();
        let mut pages = Vec::new();
        for (filename, document_pages) in documents {
            infos.push(DocumentInfo {
                filename,
                first_page: pages.len() + 1,
                pages: document_pages.len(),
                chars: document_pages.iter().map(|p| p.chars().count()).sum(),
            });
            pages.extend(document_pages);
        }
        let lowered = pages.iter().map(|page| page.to_lowercase()).collect();
        Self {
            documents: infos,
            pages,
            lowered,
        }
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn page(&self, number: usize) -> Option<&str> {
        number
            .checked_sub(1)
            .and_then(|index| self.pages.get(index))
            .map(String::as_str)
    }

    pub fn document_of(&self, page: usize) -> Option<&DocumentInfo> {
        self.documents
            .iter()
            .find(|doc| page >= doc.first_page && page < doc.first_page + doc.pages)
    }

    /// Clamp a 1-based inclusive range to the corpus.
    pub fn range(&self, from: i64, to: i64) -> Option<(usize, usize)> {
        let last = self.pages.len() as i64;
        if last == 0 {
            return None;
        }
        let from = from.clamp(1, last);
        let to = to.clamp(from, last);
        Some((from as usize, to as usize))
    }

    pub fn pages_text(&self, from: usize, to: usize) -> String {
        let mut out = String::new();
        for number in from..=to {
            if let Some(page) = self.page(number) {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("{PAGE_MARKER}{number} ---\n{page}"));
            }
        }
        out
    }

    pub fn read(&self, from: i64, to: i64, max_chars: usize) -> String {
        let Some((from, to)) = self.range(from, to) else {
            return "The documents have no pages.".into();
        };
        truncate_chars(
            &self.pages_text(from, to),
            max_chars,
            &format!("[cut at {max_chars} characters; read fewer pages to see the rest]"),
        )
    }

    fn terms(query: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        words(&query.to_lowercase())
            .filter(|word| word.chars().count() >= 2 && !STOPWORDS.contains(word))
            .filter(|word| seen.insert(word.to_string()))
            .map(str::to_string)
            .collect()
    }

    /// Pages ranked by term relevance (TF-IDF with a phrase bonus), best first.
    pub fn rank(&self, query: &str) -> Vec<(usize, f64)> {
        let terms = Self::terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let counts: Vec<HashMap<&str, usize>> = self
            .lowered
            .iter()
            .map(|page| {
                let mut counts = HashMap::new();
                for word in words(page) {
                    if terms.iter().any(|term| term == word) {
                        *counts.entry(word).or_default() += 1;
                    }
                }
                counts
            })
            .collect();
        let pages = self.pages.len() as f64;
        let idf: HashMap<&str, f64> = terms
            .iter()
            .map(|term| {
                let df = counts
                    .iter()
                    .filter(|c| c.contains_key(term.as_str()))
                    .count() as f64;
                (term.as_str(), ((pages + 1.0) / (df + 0.5)).ln().max(0.01))
            })
            .collect();
        let phrase = query.trim().to_lowercase();
        let mut ranked: Vec<(usize, f64)> = counts
            .iter()
            .enumerate()
            .filter_map(|(index, page_counts)| {
                let mut score: f64 = page_counts
                    .iter()
                    .map(|(term, tf)| idf[term] * (1.0 + (*tf as f64).ln()))
                    .sum();
                if score <= 0.0 {
                    return None;
                }
                if phrase.contains(' ') && self.lowered[index].contains(&phrase) {
                    score *= 2.0;
                }
                Some((index + 1, score))
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
    }

    pub fn search(&self, query: &str, max_chars: usize) -> String {
        let ranked = self.rank(query);
        if ranked.is_empty() {
            return format!("No page mentions {query:?}. Try other words, or read the outline.");
        }
        let terms = Self::terms(query);
        let mut out = format!(
            "{} pages match; best {}:\n",
            ranked.len(),
            ranked.len().min(SEARCH_HITS)
        );
        for (page, _) in ranked.iter().take(SEARCH_HITS) {
            let name = self
                .document_of(*page)
                .map_or("", |doc| doc.filename.as_str());
            out.push_str(&format!(
                "\n[page {page}, {name}] …{}…\n",
                self.snippet(*page, &terms)
            ));
        }
        truncate_chars(&out, max_chars, "[more results cut]")
    }

    fn snippet(&self, page: usize, terms: &[String]) -> String {
        let lowered = &self.lowered[page - 1];
        let original = &self.pages[page - 1];
        let at = terms
            .iter()
            .filter_map(|term| lowered.find(term.as_str()))
            .min()
            .unwrap_or(0);
        // Lowercasing can change byte lengths; map through a char count.
        let char_at = lowered[..at].chars().count();
        let start = char_at.saturating_sub(SNIPPET_CHARS / 3);
        original
            .chars()
            .skip(start)
            .take(SNIPPET_CHARS)
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The outline the reading model starts from, within `max_bytes`.
    pub fn outline(&self, max_bytes: usize) -> String {
        let mut out = String::new();
        for (number, doc) in self.documents.iter().enumerate() {
            out.push_str(&format!(
                "Document {} \"{}\": pages {}–{} ({} characters)\n",
                number + 1,
                crate::text::escape_attr(&doc.filename),
                doc.first_page,
                doc.first_page + doc.pages.saturating_sub(1),
                doc.chars
            ));
        }
        let lines: Vec<String> = (1..=self.pages.len())
            .map(|number| {
                let first = self.pages[number - 1]
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("(blank)");
                let first: String = first.chars().take(72).collect();
                format!("p{number}: {first}\n")
            })
            .collect();
        let room = max_bytes.saturating_sub(out.len() + 64);
        let average = lines.iter().map(String::len).sum::<usize>() / lines.len().max(1);
        let fit = (room / average.max(1)).max(1);
        let stride = lines.len().div_ceil(fit).max(1);
        out.push_str("\nFirst line of ");
        out.push_str(if stride == 1 {
            "each page:\n"
        } else {
            "sampled pages:\n"
        });
        for line in lines.iter().step_by(stride) {
            if out.len() + line.len() > max_bytes {
                break;
            }
            out.push_str(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Corpus {
        let first: Vec<String> = (1..=40)
            .map(|n| format!("Chapter {n}\nThe river flows past the mill on day {n}."))
            .collect();
        let mut second: Vec<String> = (1..=10).map(|n| format!("Appendix {n} tables")).collect();
        second[6] = "Appendix 7\nThe launch code is AZURE-HERON-42, kept in the vault.".into();
        Corpus::new(vec![("a.pdf".into(), first), ("b.pdf".into(), second)])
    }

    #[test]
    fn pages_are_numbered_across_documents() {
        let corpus = corpus();
        assert_eq!(corpus.page_count(), 50);
        assert_eq!(corpus.document_of(47).unwrap().filename, "b.pdf");
        assert!(corpus.page(47).unwrap().contains("AZURE-HERON-42"));
        assert_eq!(corpus.range(-3, 999), Some((1, 50)));
    }

    #[test]
    fn search_finds_a_fact_on_one_late_page() {
        let corpus = corpus();
        assert_eq!(corpus.rank("what is the launch code")[0].0, 47);
        let result = corpus.search("launch code vault", 8_000);
        assert!(result.contains("[page 47, b.pdf]"));
        assert!(result.contains("AZURE-HERON-42"));
    }

    #[test]
    fn tool_results_respect_the_character_limit() {
        let corpus = corpus();
        let read = corpus.read(1, 50, 500);
        assert!(read.chars().count() <= 500, "{}", read.chars().count());
        assert!(read.ends_with("read fewer pages to see the rest]"));
        assert!(corpus.search("river mill", 300).chars().count() <= 300);
    }

    #[test]
    fn the_outline_fits_its_budget() {
        let corpus = corpus();
        let outline = corpus.outline(600);
        assert!(outline.len() <= 600);
        assert!(outline.contains("Document 2 \"b.pdf\": pages 41–50"));
        assert!(outline.contains("sampled pages"));
        assert!(corpus.outline(100_000).contains("p47: Appendix 7"));
    }
}
