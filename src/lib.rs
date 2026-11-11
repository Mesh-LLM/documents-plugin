//! `pdf`: PDF attachments for Mesh LLM, decoded with Apache Tika (and OCR).
//!
//! - `transform`: a request transform (`request_transform.v1`) that replaces
//!   PDF attachments in inference requests with their text, on the node the
//!   client called, before routing. Clients keep their usual base URL.
//! - `extract`: the same extraction as a tool, over MCP (`pdf.extract` on the
//!   console's `/mcp`) and HTTP (`/api/plugins/pdf/tools/extract`).

mod attachments;
mod error;
mod extract;
mod settings;
mod store;
mod text;
pub mod tika;
mod transform;

#[cfg(test)]
mod test_support;

use mesh_llm_plugin::{
    OperationRouter, PluginMetadata, PluginRuntime, SimplePlugin, config_boolean, config_integer,
    config_path, config_schema, config_setting, config_string, json_schema_operation, operation,
    plugin_server_info,
};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub use error::DocError;
pub use settings::Settings;

const SWEEP_EVERY: Duration = Duration::from_secs(60);

const EXTRACT_DESCRIPTION: &str = "Extract the text of a PDF (OCR for scanned pages). Give `path` \
(absolute, on this machine) or `data_base64`. Small documents return `text` with `--- Page N ---` \
markers; large ones return `text_path` (read or grep it) plus a `preview`. `output`: auto | text | file.";

#[derive(Default)]
pub struct Stats {
    pub extractions: AtomicU64,
    pub cache_hits: AtomicU64,
}

pub struct Pdf {
    settings: Settings,
    store: store::Store,
    tika: tika::Tika,
    stats: Stats,
    last_sweep: Mutex<Option<std::time::Instant>>,
}

impl Pdf {
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        let store = store::Store::open(
            settings.store_root()?,
            settings.ttl(),
            settings.max_store_bytes(),
        )?;
        let tika = tika::Tika::new(&settings)?;
        Ok(Self {
            settings,
            store,
            tika,
            stats: Stats::default(),
            last_sweep: Mutex::new(None),
        })
    }

    fn maybe_sweep(&self) {
        let mut last = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
        drop(last);
        self.store.sweep();
    }

    fn status(&self) -> StatusReport {
        StatusReport {
            extractions: self.stats.extractions.load(Ordering::Relaxed),
            cache_hits: self.stats.cache_hits.load(Ordering::Relaxed),
            ocr: self.settings.ocr,
        }
    }
}

#[derive(Serialize)]
struct StatusReport {
    extractions: u64,
    cache_hits: u64,
    ocr: bool,
}

type Shared = Arc<RwLock<Option<Arc<Pdf>>>>;

fn current(shared: &Shared) -> Result<Arc<Pdf>, mesh_llm_plugin::PluginError> {
    shared
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| mesh_llm_plugin::PluginError::internal("pdf plugin is not initialized"))
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoArgs {}

pub fn plugin() -> SimplePlugin {
    let name = std::env::var("MESH_LLM_PLUGIN_NAME").unwrap_or_else(|_| "pdf".into());
    let version = env!("CARGO_PKG_VERSION");
    let shared: Shared = Arc::default();
    SimplePlugin::new(PluginMetadata::new(
        name.clone(),
        version,
        plugin_server_info(
            name.clone(),
            version,
            "PDF",
            "PDF text extraction with Apache Tika, including OCR",
            None::<String>,
        ),
    ))
    .with_manifest(manifest(&name))
    .with_operation_router(operations(&shared))
    .with_request_transform({
        let shared = Arc::clone(&shared);
        move |input, _context| {
            let shared = Arc::clone(&shared);
            Box::pin(async move {
                let pdf = current(&shared)?;
                transform::transform(&pdf, input)
                    .await
                    .map_err(transform::plugin_error)
            })
        }
    })
    .on_initialize(move |request, _context| {
        let shared = Arc::clone(&shared);
        Box::pin(async move {
            let settings = Settings::parse(&request.settings_json)
                .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
            let pdf = Pdf::new(settings)
                .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
            *shared.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(pdf));
            Ok(())
        })
    })
}

fn manifest(name: &str) -> mesh_llm_plugin::proto::PluginManifest {
    let setting =
        |key: &str, schema, description: &str| config_setting(key, schema).description(description);
    mesh_llm_plugin::plugin_manifest![
        operation::<extract::ExtractArgs>("extract", EXTRACT_DESCRIPTION),
        operation::<NoArgs>("status", "Extraction and cache counters."),
        mesh_llm_plugin::request_transform::request_transform(),
        config_schema(name)
            .setting(
                setting(
                    "inline_max_chars",
                    config_integer(),
                    "Return text directly up to this many characters; larger documents return a file path."
                )
                .default_value(&40_000)
            )
            .setting(setting(
                "store_dir",
                config_path(),
                "Where extracted documents are kept (default ~/.mesh-llm/pdf)."
            ))
            .setting(
                setting("ttl_hours", config_integer(), "Remove extractions unused for this long.")
                    .default_value(&24)
            )
            .setting(
                setting("max_store_mb", config_integer(), "Size cap for stored extractions.")
                    .default_value(&2048)
            )
            .setting(
                setting("max_pdf_mb", config_integer(), "Largest PDF accepted.")
                    .default_value(&64)
            )
            .setting(
                setting("timeout_secs", config_integer(), "Per-PDF extraction limit.")
                    .default_value(&300)
            )
            .setting(
                setting("tika_concurrency", config_integer(), "Concurrent extractions.")
                    .default_value(&2)
            )
            .setting(
                setting("ocr", config_boolean(), "OCR pages without a text layer (needs tesseract).")
                    .default_value(&true)
            )
            .setting(
                setting("ocr_language", config_string(), "Tesseract language(s), e.g. eng or eng+deu.")
                    .default_value(&"eng")
            )
            .setting(setting(
                "tesseract_dir",
                config_path(),
                "Directory containing the tesseract binary, if it is not on PATH."
            )),
    ]
}

fn operations(shared: &Shared) -> OperationRouter {
    let mut router = OperationRouter::new();
    let for_extract = Arc::clone(shared);
    router.add_json::<extract::ExtractArgs, extract::Extracted, _>(
        json_schema_operation::<extract::ExtractArgs>("extract", EXTRACT_DESCRIPTION),
        move |args, _context| {
            let shared = Arc::clone(&for_extract);
            Box::pin(async move {
                current(&shared)?.extract(args).await.map_err(|error| {
                    mesh_llm_plugin::PluginError::invalid_params(error.to_string())
                })
            })
        },
    );
    let for_status = Arc::clone(shared);
    router.add_json::<NoArgs, StatusReport, _>(
        json_schema_operation::<NoArgs>("status", "Extraction and cache counters."),
        move |_args, _context| {
            let shared = Arc::clone(&for_status);
            Box::pin(async move { Ok(current(&shared)?.status()) })
        },
    );
    router
}

pub async fn run() -> anyhow::Result<()> {
    PluginRuntime::run(plugin()).await
}
