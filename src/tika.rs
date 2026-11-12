//! Apache Tika without a JVM: Tika compiled to a native shared library with
//! GraalVM native-image ([extractous](https://github.com/yobix-ai/extractous),
//! `libtika_native`).
//!
//! Each extraction runs in a child process (this same binary with
//! [`WORKER_FLAG`]): the PDF goes in on stdin, Tika's XHTML comes back on
//! stdout. So a deadline can kill it, a crash or leak inside Tika can't take
//! the plugin down, and the child finds `tesseract` through its own PATH.
//! With `ocr` on, Tika OCRs pages that have no text layer with Tesseract.

use crate::error::DocError;
use crate::settings::Settings;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Argument that turns this binary into a one-shot extraction worker.
pub const WORKER_FLAG: &str = "--extract-worker";
/// Worker exit code for a PDF Tika could not parse (damaged, encrypted, not a PDF).
const EXIT_INVALID_PDF: u8 = 3;
/// Worker exit code when its own deadline passed.
const EXIT_TIMEOUT: u8 = 124;
/// Tika's output cap, in characters (its default is 100k).
const MAX_OUTPUT_CHARS: i32 = 256 * 1024 * 1024;

pub struct Tika {
    program: PathBuf,
    args: Vec<OsString>,
    path: Option<OsString>,
    permits: Semaphore,
}

impl Tika {
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        let mut args = vec![OsString::from(WORKER_FLAG)];
        if settings.ocr {
            args.push("--ocr".into());
            args.push(settings.ocr_language.clone().into());
        }
        Ok(Self {
            program: std::env::current_exe()?,
            args,
            path: worker_path(settings.tesseract_dir.as_deref()),
            permits: Semaphore::new(settings.tika_concurrency),
        })
    }

    /// Extract one PDF into page texts, or fail by `deadline`.
    pub async fn extract(&self, pdf: Vec<u8>, deadline: Instant) -> Result<Vec<String>, DocError> {
        // Dropping the future at the deadline drops the child, which kills it.
        tokio::time::timeout_at(deadline, self.extract_inner(pdf, deadline))
            .await
            .map_err(|_| DocError::timeout())?
    }

    async fn extract_inner(
        &self,
        pdf: Vec<u8>,
        deadline: Instant,
    ) -> Result<Vec<String>, DocError> {
        let _permit = self.permits.acquire().await.map_err(DocError::internal)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            // The worker also stops itself, in case this process dies first.
            .args(["--deadline-secs", &remaining.as_secs().max(1).to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        let mut child = command
            .spawn()
            .map_err(|error| unavailable(format!("could not start the Tika worker: {error}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| unavailable("no worker stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable("no worker stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| unavailable("no worker stderr"))?;
        let feed = async move {
            // A worker that exits early closes the pipe; its exit status says why.
            let _ = stdin.write_all(&pdf).await;
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (_, read_out, read_err) = tokio::join!(
            feed,
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        read_out.map_err(|error| unavailable(format!("Tika worker output failed: {error}")))?;
        let _ = read_err;
        let status = child
            .wait()
            .await
            .map_err(|error| unavailable(format!("Tika worker failed: {error}")))?;
        match status.code() {
            Some(0) => Ok(crate::text::pages_from_xhtml(&String::from_utf8_lossy(
                &out,
            ))),
            Some(code) if code == i32::from(EXIT_INVALID_PDF) => Err(DocError::new(
                422,
                "invalid_pdf",
                "the PDF could not be read (damaged, encrypted or unsupported)",
            )),
            Some(code) if code == i32::from(EXIT_TIMEOUT) => Err(DocError::timeout()),
            _ => {
                let err = String::from_utf8_lossy(&err);
                let tail: String = err.trim().chars().rev().take(300).collect();
                let tail: String = tail.chars().rev().collect();
                Err(unavailable(format!(
                    "Tika worker failed ({status}): {tail}"
                )))
            }
        }
    }

    #[cfg(test)]
    fn with_program(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            path: None,
            permits: Semaphore::new(1),
        }
    }
}

/// PATH for the worker: `tesseract_dir` first, then the plugin's PATH, then
/// the usual Homebrew/local bins, or Tesseract's default Windows install
/// folder (a GUI-launched node has a bare PATH).
fn worker_path(tesseract_dir: Option<&std::path::Path>) -> Option<OsString> {
    let mut dirs: Vec<PathBuf> = tesseract_dir.map(PathBuf::from).into_iter().collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    #[cfg(windows)]
    let extras = [r"C:\Program Files\Tesseract-OCR"];
    #[cfg(not(windows))]
    let extras = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
    for extra in extras {
        let extra = PathBuf::from(extra);
        if extra.is_dir() && !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    std::env::join_paths(dirs).ok()
}

/// The worker process: `--extract-worker [--ocr <lang>] [--deadline-secs <n>]`,
/// PDF on stdin, XHTML on stdout.
pub fn run_worker(args: &[String]) -> ExitCode {
    use extractous::{Extractor, PdfOcrStrategy, PdfParserConfig, TesseractOcrConfig};

    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|at| args.get(at + 1))
    };
    if let Some(secs) = value("--deadline-secs").and_then(|secs| secs.parse::<u64>().ok()) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            std::process::exit(i32::from(EXIT_TIMEOUT));
        });
    }
    let mut pdf = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut pdf) {
        eprintln!("reading the PDF from stdin: {error}");
        return ExitCode::FAILURE;
    }
    let ocr = value("--ocr");
    let strategy = if ocr.is_some() {
        PdfOcrStrategy::AUTO
    } else {
        PdfOcrStrategy::NO_OCR
    };
    let mut extractor = Extractor::new()
        .set_xml_output(true)
        .set_extract_string_max_length(MAX_OUTPUT_CHARS)
        .set_pdf_config(PdfParserConfig::new().set_ocr_strategy(strategy));
    if let Some(language) = ocr {
        extractor = extractor.set_ocr_config(TesseractOcrConfig::new().set_language(language));
    }
    match extractor.extract_bytes_to_string(&pdf) {
        Ok((xhtml, _metadata)) => {
            let mut stdout = std::io::stdout().lock();
            if stdout
                .write_all(xhtml.as_bytes())
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(extractous::Error::ParseError(message)) => {
            eprintln!("{message}");
            ExitCode::from(EXIT_INVALID_PDF)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn unavailable(message: impl Into<String>) -> DocError {
    DocError::new(503, "document_extractor_unavailable", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker that never answers: the deadline must end the call, and kill it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_worker_is_killed_at_the_deadline() {
        let tika = Tika::with_program("/bin/sh", &["-c", "sleep 30", "sh"]);
        let started = Instant::now();
        let error = tika
            .extract(
                b"%PDF-1.7".to_vec(),
                Instant::now() + Duration::from_millis(300),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 504);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_worker_is_unavailable() {
        let tika = Tika::with_program("/bin/sh", &["-c", "echo boom >&2; exit 1", "sh"]);
        let error = tika
            .extract(
                b"%PDF-1.7".to_vec(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (error.status, error.code.as_str()),
            (503, "document_extractor_unavailable")
        );
        assert!(error.message.contains("boom"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_missing_worker_is_unavailable() {
        let tika = Tika::with_program("/nonexistent/pdf-worker", &[]);
        let error = tika
            .extract(
                b"%PDF-1.7".to_vec(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 503);
    }
}
