//! Extracted documents on disk, keyed by the PDF's SHA-256.
//!
//! `<root>/<id>/` holds `original.pdf`, `text.md` (page markers),
//! `pages.json` and `meta.json`. Directories are 0700 and files 0600. A
//! re-sent PDF is found by its hash and not extracted again. Entries unused
//! for the TTL are removed, oldest first when over the size cap.

use crate::error::DocError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Meta {
    pub filename: String,
    pub pages: usize,
    pub chars: usize,
    pub pdf_bytes: u64,
    pub last_used: u64,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub dir: PathBuf,
    pub meta: Meta,
}

impl Entry {
    pub fn text_path(&self) -> PathBuf {
        self.dir.join("text.md")
    }

    pub fn pdf_path(&self) -> PathBuf {
        self.dir.join("original.pdf")
    }
}

pub struct Store {
    root: PathBuf,
    ttl: Duration,
    max_bytes: u64,
}

/// The first 96 bits of the PDF's SHA-256: unique within one store, and
/// short enough for a small model to copy a pointer path correctly.
pub fn document_id(pdf: &[u8]) -> String {
    hex::encode(&Sha256::digest(pdf)[..ID_BYTES])
}

const ID_BYTES: usize = 12;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Owner-only on Unix. On Windows the default ACL under the user's profile
/// already limits it to that user.
fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

impl Store {
    pub fn open(root: PathBuf, ttl: Duration, max_bytes: u64) -> std::io::Result<Self> {
        private_dir(&root)?;
        Ok(Self {
            root,
            ttl,
            max_bytes,
        })
    }

    fn valid_id(id: &str) -> bool {
        id.len() == ID_BYTES * 2
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// A cached extraction, marked as used now.
    pub fn lookup(&self, id: &str) -> Option<Entry> {
        if !Self::valid_id(id) {
            return None;
        }
        let dir = self.root.join(id);
        let mut meta: Meta =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
        if !dir.join("pages.json").is_file() || !dir.join("text.md").is_file() {
            return None;
        }
        meta.last_used = now_secs();
        let _ = write_private(&dir.join("meta.json"), &serde_json::to_vec(&meta).ok()?);
        Some(Entry {
            id: id.into(),
            dir,
            meta,
        })
    }

    pub fn save(
        &self,
        id: &str,
        pdf: &[u8],
        filename: &str,
        pages: &[String],
    ) -> Result<Entry, DocError> {
        let meta = Meta {
            filename: filename.into(),
            pages: pages.len(),
            chars: pages.iter().map(|page| page.chars().count()).sum(),
            pdf_bytes: pdf.len() as u64,
            last_used: now_secs(),
        };
        let temp = self
            .root
            .join(format!(".tmp-{}", hex::encode(rand::random::<[u8; 8]>())));
        let written = (|| -> std::io::Result<()> {
            private_dir(&temp)?;
            write_private(&temp.join("original.pdf"), pdf)?;
            write_private(
                &temp.join("text.md"),
                crate::text::text_with_page_markers(pages).as_bytes(),
            )?;
            write_private(&temp.join("pages.json"), &serde_json::to_vec(pages)?)?;
            write_private(&temp.join("meta.json"), &serde_json::to_vec(&meta)?)?;
            Ok(())
        })();
        let dir = self.root.join(id);
        let result = written.and_then(|()| match std::fs::rename(&temp, &dir) {
            // Another request stored the same PDF first; theirs is identical.
            Err(_) if dir.join("meta.json").is_file() => Ok(()),
            other => other,
        });
        let _ = std::fs::remove_dir_all(&temp);
        result.map_err(|error| DocError::internal(format!("document store: {error}")))?;
        Ok(Entry {
            id: id.into(),
            dir,
            meta,
        })
    }

    pub fn pages(&self, id: &str) -> Option<Vec<String>> {
        if !Self::valid_id(id) {
            return None;
        }
        serde_json::from_slice(&std::fs::read(self.root.join(id).join("pages.json")).ok()?).ok()
    }

    /// Remove expired entries, then the least recently used ones over the cap.
    pub fn sweep(&self) -> usize {
        let Ok(dirs) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let now = now_secs();
        let mut live = Vec::new();
        let mut removed = 0;
        for dir in dirs.flatten() {
            let name = dir.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                // Left by a crash mid-save; nothing refers to it.
                let stale = dir
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m.elapsed().unwrap_or_default() > Duration::from_secs(3600));
                if stale && std::fs::remove_dir_all(dir.path()).is_ok() {
                    removed += 1;
                }
                continue;
            }
            if !Self::valid_id(&name) {
                continue;
            }
            let meta: Option<Meta> = std::fs::read(dir.path().join("meta.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            let last_used = meta.as_ref().map_or(0, |meta| meta.last_used);
            if now.saturating_sub(last_used) >= self.ttl.as_secs() {
                if std::fs::remove_dir_all(dir.path()).is_ok() {
                    removed += 1;
                }
                continue;
            }
            live.push((last_used, dir_size(&dir.path()), dir.path()));
        }
        let mut total: u64 = live.iter().map(|(_, size, _)| size).sum();
        live.sort_by_key(|(last_used, _, _)| *last_used);
        for (_, size, path) in live {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                total = total.saturating_sub(size);
                removed += 1;
            }
        }
        removed
    }
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .map(|metadata| metadata.len())
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn pages() -> Vec<String> {
        vec!["one".into(), "two".into()]
    }

    #[test]
    fn saved_documents_are_private_and_found_by_hash() {
        let dir = TempDir::new();
        let store =
            Store::open(dir.path().join("docs"), Duration::from_secs(3600), u64::MAX).unwrap();
        let pdf = b"%PDF-1.7 a".to_vec();
        let id = document_id(&pdf);
        assert!(store.lookup(&id).is_none());
        let entry = store.save(&id, &pdf, "a.pdf", &pages()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&entry.dir), 0o700);
            assert_eq!(mode(&entry.text_path()), 0o600);
        }
        assert_eq!(std::fs::read(entry.pdf_path()).unwrap(), pdf);
        let text = std::fs::read_to_string(entry.text_path()).unwrap();
        assert!(text.contains("--- Page 2 ---\n\ntwo"));
        assert_eq!(store.lookup(&id).unwrap().meta.pages, 2);
        assert_eq!(store.pages(&id).unwrap(), pages());
    }

    #[test]
    fn the_sweep_removes_expired_entries() {
        let dir = TempDir::new();
        let store = Store::open(dir.path().join("docs"), Duration::ZERO, u64::MAX).unwrap();
        let a = document_id(b"a");
        store.save(&a, b"a", "a.pdf", &pages()).unwrap();
        assert_eq!(store.sweep(), 1);
        assert!(store.lookup(&a).is_none());
    }

    #[test]
    fn over_the_size_cap_the_least_recently_used_goes_first() {
        let dir = TempDir::new();
        let store = Store::open(dir.path().join("docs"), Duration::from_secs(3600), 1).unwrap();
        let a = document_id(b"a");
        store.save(&a, b"a", "a.pdf", &pages()).unwrap();
        assert_eq!(store.sweep(), 1);
        assert!(store.lookup(&a).is_none());
    }
}
