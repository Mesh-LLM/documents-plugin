//! Operator settings (`[plugin.settings]`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Return text directly up to this many characters; larger documents
    /// come back as a file path.
    pub inline_max_chars: usize,
    pub store_dir: Option<PathBuf>,
    pub ttl_hours: u64,
    pub max_store_mb: u64,
    pub max_pdf_mb: u64,
    /// Per-extraction limit; OCR of a long scan is slow.
    pub timeout_secs: u64,
    pub tika_concurrency: usize,
    /// OCR pages without a text layer (needs Tesseract).
    pub ocr: bool,
    pub ocr_language: String,
    /// Directory holding the `tesseract` binary, put first on the worker's PATH.
    pub tesseract_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            inline_max_chars: 40_000,
            store_dir: None,
            ttl_hours: 24,
            max_store_mb: 2048,
            max_pdf_mb: 64,
            timeout_secs: 300,
            tika_concurrency: 2,
            ocr: true,
            ocr_language: "eng".into(),
            tesseract_dir: None,
        }
    }
}

impl Settings {
    pub fn parse(settings_json: &str) -> Result<Self> {
        let settings: Self = if settings_json.trim().is_empty() {
            Self::default()
        } else {
            serde_json::from_str(settings_json).context("invalid pdf plugin settings")?
        };
        if settings.tika_concurrency == 0 {
            bail!("tika_concurrency must be at least 1");
        }
        if settings.ttl_hours == 0 {
            bail!("ttl_hours must be at least 1");
        }
        if settings.timeout_secs == 0 {
            bail!("timeout_secs must be at least 1");
        }
        if !settings.ocr_language.split('+').all(|lang| {
            !lang.is_empty() && lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            bail!("ocr_language must look like eng or eng+deu");
        }
        Ok(settings)
    }

    pub fn store_root(&self) -> Result<PathBuf> {
        if let Some(dir) = self
            .store_dir
            .as_ref()
            .filter(|dir| !dir.as_os_str().is_empty())
        {
            if !dir.is_absolute() {
                bail!("store_dir must be an absolute path");
            }
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .context("HOME is not set; configure store_dir")?;
        Ok(PathBuf::from(home).join(".mesh-llm").join("pdf"))
    }

    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_hours.saturating_mul(3600))
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    pub fn max_pdf_bytes(&self) -> u64 {
        self.max_pdf_mb.saturating_mul(1024 * 1024)
    }

    pub fn max_store_bytes(&self) -> u64 {
        self.max_store_mb.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_settings_use_the_defaults() {
        let settings = Settings::parse("{}").unwrap();
        assert_eq!(settings.inline_max_chars, 40_000);
        assert!(settings.ocr);
        assert_eq!(settings.ocr_language, "eng");
    }

    #[test]
    fn bad_settings_are_rejected() {
        assert!(Settings::parse(r#"{"ocrr":true}"#).is_err());
        assert!(Settings::parse(r#"{"ocr_language":"eng; rm"}"#).is_err());
        assert!(Settings::parse(r#"{"ocr_language":"eng+deu"}"#).is_ok());
        let settings = Settings::parse(r#"{"store_dir":"rel/dir"}"#).unwrap();
        assert!(settings.store_root().is_err());
    }
}
