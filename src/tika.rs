//! Apache Tika on loopback: either an operator-run server (`tika_url`) or one
//! this plugin starts from `tika_jar` and stops with itself.
//!
//! Every extraction runs under the caller's deadline. When it passes, the
//! HTTP request is dropped (closing the connection), Tika is told the same
//! deadline through `X-Tika-Timeout-Millis`, and the caller gets a 504. The
//! With `ocr` on, Tika OCRs pages that have no text layer with Tesseract
//! (`X-Tika-PDFOcrStrategy: auto`).

use crate::error::DocError;
use crate::settings::Settings;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

const STARTUP_LIMIT: Duration = Duration::from_secs(60);

pub struct Tika {
    source: Source,
    client: reqwest::Client,
    server: Mutex<Option<Server>>,
    permits: Semaphore,
    ocr: Option<String>,
}

enum Source {
    Url(String),
    Jar {
        java: String,
        jar: PathBuf,
        tesseract_dir: Option<PathBuf>,
    },
    Missing,
}

struct Server {
    url: String,
    child: Option<Child>,
}

impl Tika {
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        let source = match (&settings.tika_url, &settings.tika_jar) {
            (Some(url), _) if !url.trim().is_empty() => Source::Url(loopback_url(url)?),
            (_, Some(jar)) if !jar.as_os_str().is_empty() => Source::Jar {
                java: settings.java.clone(),
                jar: jar.clone(),
                tesseract_dir: settings.tesseract_dir.clone(),
            },
            _ => Source::Missing,
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            source,
            client,
            server: Mutex::new(None),
            permits: Semaphore::new(settings.tika_concurrency),
            ocr: settings.ocr.then(|| settings.ocr_language.clone()),
        })
    }

    /// Extract one PDF into page texts, or fail by `deadline`.
    pub async fn extract(&self, pdf: Vec<u8>, deadline: Instant) -> Result<Vec<String>, DocError> {
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
        let url = self.server_url().await?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut request = self
            .client
            .put(format!("{url}/tika"))
            .header("Accept", "text/html")
            .header("Content-Type", "application/pdf")
            .header("X-Tika-Timeout-Millis", remaining.as_millis().to_string());
        request = match &self.ocr {
            Some(language) => request
                .header("X-Tika-PDFOcrStrategy", "auto")
                .header("X-Tika-OCRLanguage", language),
            None => request
                .header("X-Tika-PDFOcrStrategy", "no_ocr")
                .header("X-Tika-OCRskipOcr", "true"),
        };
        let response = request
            .body(pdf)
            .send()
            .await
            .map_err(|error| unavailable(format!("Tika request failed: {error}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| unavailable(format!("Tika response failed: {error}")))?;
        match status.as_u16() {
            200 => Ok(crate::text::pages_from_xhtml(&body)),
            204 => Ok(Vec::new()),
            415 | 422 => Err(DocError::new(
                422,
                "invalid_pdf",
                "the PDF could not be read (damaged, encrypted or unsupported)",
            )),
            other => Err(unavailable(format!("Tika answered HTTP {other}"))),
        }
    }

    async fn server_url(&self) -> Result<String, DocError> {
        let mut server = self.server.lock().await;
        if let Some(running) = server.as_mut() {
            let exited = running
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some());
            if !exited {
                return Ok(running.url.clone());
            }
            *server = None;
        }
        let started = match &self.source {
            Source::Url(url) => Server {
                url: url.clone(),
                child: None,
            },
            Source::Jar {
                java,
                jar,
                tesseract_dir,
            } => self.start(java, jar, tesseract_dir.as_deref()).await?,
            Source::Missing => {
                return Err(unavailable(
                    "no PDF extractor configured: set tika_url or tika_jar",
                ));
            }
        };
        let url = started.url.clone();
        *server = Some(started);
        Ok(url)
    }

    async fn start(
        &self,
        java: &str,
        jar: &PathBuf,
        tesseract_dir: Option<&std::path::Path>,
    ) -> Result<Server, DocError> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .map_err(DocError::internal)?
            .port();
        let mut command = Command::new(java);
        // Tika's watchdog forks its worker with `java` from PATH, and finds
        // `tesseract` on PATH: put the configured Java and Tesseract first, and
        // the usual Homebrew/local bins last (a GUI-launched node has a bare PATH).
        let java_bin = std::path::Path::new(java)
            .parent()
            .filter(|dir| dir.is_absolute());
        let mut dirs: Vec<PathBuf> = java_bin.into_iter().map(PathBuf::from).collect();
        dirs.extend(tesseract_dir.map(PathBuf::from));
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            let extra = PathBuf::from(extra);
            if extra.is_dir() && !dirs.contains(&extra) {
                dirs.push(extra);
            }
        }
        if let Ok(joined) = std::env::join_paths(dirs) {
            command.env("PATH", joined);
        }
        if let Some(home) = java_bin.and_then(|bin| bin.parent()) {
            command.env("JAVA_HOME", home);
        }
        let mut child = command
            .arg("-jar")
            .arg(jar)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| unavailable(format!("could not start Tika: {error}")))?;
        let url = format!("http://127.0.0.1:{port}");
        let ready_by = Instant::now() + STARTUP_LIMIT;
        while Instant::now() < ready_by {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(unavailable(format!("Tika exited during startup: {status}")));
            }
            let probe = self.client.get(format!("{url}/version")).send().await;
            if probe.is_ok_and(|response| response.status().is_success()) {
                return Ok(Server {
                    url,
                    child: Some(child),
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(unavailable("Tika did not become ready"))
    }
}

fn unavailable(message: impl Into<String>) -> DocError {
    DocError::new(503, "document_extractor_unavailable", message)
}

/// Tika must only ever be reached on this machine.
fn loopback_url(url: &str) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(url.trim())?;
    let loopback = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if parsed.scheme() != "http" || !loopback {
        anyhow::bail!("tika_url must be an http:// loopback address");
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tika_url_must_be_loopback() {
        assert_eq!(
            loopback_url("http://127.0.0.1:9998/").unwrap(),
            "http://127.0.0.1:9998"
        );
        assert!(loopback_url("http://localhost:9998").is_ok());
        assert!(loopback_url("http://[::1]:9998").is_ok());
        assert!(loopback_url("http://10.0.0.2:9998").is_err());
        assert!(loopback_url("https://127.0.0.1:9998").is_err());
        assert!(loopback_url("http://tika.example.com").is_err());
    }

    /// A Tika that accepts the request and never answers: the plugin's own
    /// deadline must end the call and close the connection.
    #[tokio::test]
    async fn a_hung_tika_is_abandoned_at_the_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 64 * 1024];
            // Read until the client gives up and closes its side.
            while socket.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
            let _ = closed_tx.send(());
        });
        let settings = Settings {
            tika_url: Some(url),
            ..Settings::default()
        };
        let tika = Tika::new(&settings).unwrap();
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
        tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("the plugin closed its Tika connection")
            .unwrap();
    }

    #[tokio::test]
    async fn a_stopped_tika_is_unavailable() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = Settings {
            tika_url: Some(format!("http://127.0.0.1:{port}")),
            ..Settings::default()
        };
        let tika = Tika::new(&settings).unwrap();
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
    }
}
