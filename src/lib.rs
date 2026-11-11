//! `documents`: PDF attachments for Mesh LLM.
//!
//! The host hands this plugin every PDF part of a local request
//! (`document-ingest.v1`). Small documents come back as inline text, large
//! ones as file paths for tool-using local agents, and otherwise as a token
//! for the `mesh-docs` virtual model, which reads the document with a
//! bounded tool loop and answers.

mod error;
mod prepare;
mod rlm;
mod settings;
mod staging;
mod store;
mod text;
mod tika;
mod tokens;

#[cfg(test)]
mod test_support;

use mesh_llm_plugin::{
    OperationRouter, PluginContext, PluginMetadata, PluginRuntime, SimplePlugin,
    VirtualModelInvocation, VirtualModelRouter, capability, config_enum, config_integer,
    config_path, config_schema, config_setting, config_string, json_schema_operation, operation,
    operation_with_schema, plugin_server_info, structured_tool_result, virtual_model,
};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub use error::DocError;
pub use settings::{PointerPolicy, Settings};

pub const CAPABILITY: &str = "document-ingest.v1";
pub const VIRTUAL_MODEL: &str = "mesh-docs";
const HANDLER: &str = "chat";
const SWEEP_EVERY: Duration = Duration::from_secs(60);

pub const PROGRESS_LINES: [&str; 5] = [
    "Opening the document…",
    "Searching the document…",
    "Reading relevant pages…",
    "Checking details…",
    "Still reading…",
];

/// Counters, readable through the `status` operation.
#[derive(Default)]
pub struct Stats {
    pub extractions: AtomicU64,
    pub cache_hits: AtomicU64,
    pub rlm_turns: AtomicU64,
    last_turn: Mutex<Option<rlm::TurnReport>>,
}

pub struct Documents {
    settings: Settings,
    staging: staging::Staging,
    store: store::Store,
    tika: tika::Tika,
    tokens: tokens::Tokens,
    stats: Stats,
    last_sweep: Mutex<Option<std::time::Instant>>,
}

impl Documents {
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        let root = settings.store_root()?;
        let staging = staging::Staging::create(root.join("staging"))?;
        let store = store::Store::open(
            root.join("docs"),
            settings.ttl(),
            settings.max_store_bytes(),
        )?;
        let tika = tika::Tika::new(&settings)?;
        Ok(Self {
            settings,
            staging,
            store,
            tika,
            tokens: tokens::Tokens::default(),
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
            rlm_turns: self.stats.rlm_turns.load(Ordering::Relaxed),
            live_tokens: self.tokens.len(),
            last_rlm_turn: self
                .stats
                .last_turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
}

#[derive(Serialize)]
struct StatusReport {
    extractions: u64,
    cache_hits: u64,
    rlm_turns: u64,
    live_tokens: usize,
    last_rlm_turn: Option<rlm::TurnReport>,
}

type Shared = Arc<RwLock<Option<Arc<Documents>>>>;

fn current(shared: &Shared) -> Result<Arc<Documents>, mesh_llm_plugin::PluginError> {
    shared
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| {
            mesh_llm_plugin::PluginError::internal("documents plugin is not initialized")
        })
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoArgs {}

pub fn plugin() -> SimplePlugin {
    let name = std::env::var("MESH_LLM_PLUGIN_NAME").unwrap_or_else(|_| "documents".into());
    let version = env!("CARGO_PKG_VERSION");
    let shared: Shared = Arc::default();
    SimplePlugin::new(PluginMetadata::new(
        name.clone(),
        version,
        plugin_server_info(
            name.clone(),
            version,
            "Documents",
            "PDF attachments: inline text, local files for agents, or the mesh-docs reading model",
            None::<String>,
        ),
    ))
    .with_manifest(manifest(&name))
    .with_operation_router(operations(&shared))
    .with_virtual_model_router(virtual_models(&shared))
    .on_initialize(move |request, _context| {
        let shared = Arc::clone(&shared);
        Box::pin(async move {
            let settings = Settings::parse(&request.settings_json)
                .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
            let documents = Documents::new(settings)
                .map_err(|error| mesh_llm_plugin::PluginError::internal(error.to_string()))?;
            *shared.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(documents));
            Ok(())
        })
    })
}

fn manifest(name: &str) -> mesh_llm_plugin::proto::PluginManifest {
    let setting =
        |key: &str, schema, description: &str| config_setting(key, schema).description(description);
    mesh_llm_plugin::plugin_manifest![
        capability(CAPABILITY),
        // Only `status` is published (HTTP and MCP); the ingest operations
        // stay host-internal behind the capability.
        operation::<NoArgs>("status", "Extraction, cache and mesh-docs counters."),
        virtual_model(VIRTUAL_MODEL, HANDLER)
            .supports_streaming(true)
            .requires_candidates(true)
            .progress_lines(PROGRESS_LINES),
        config_schema(name)
            .setting(
                setting(
                    "inline_max_chars",
                    config_integer(),
                    "Inline documents up to this many characters of text."
                )
                .default_value(&40_000)
            )
            .setting(
                setting(
                    "pointer",
                    config_enum(["auto", "header", "off"]),
                    "When large documents go to tool-using local agents as file paths."
                )
                .default_value(&"auto")
            )
            .setting(setting(
                "store_dir",
                config_path(),
                "Where extracted documents are kept (default ~/.mesh-llm/documents)."
            ))
            .setting(
                setting(
                    "ttl_hours",
                    config_integer(),
                    "Remove stored documents unused for this long."
                )
                .default_value(&24)
            )
            .setting(
                setting(
                    "max_store_mb",
                    config_integer(),
                    "Size cap for stored documents."
                )
                .default_value(&2048)
            )
            .setting(
                setting(
                    "max_total_pdf_mb",
                    config_integer(),
                    "Largest total PDF size per request."
                )
                .default_value(&64)
            )
            .setting(
                setting(
                    "max_total_chars",
                    config_integer(),
                    "Largest total extracted text per request."
                )
                .default_value(&5_000_000)
            )
            .setting(setting(
                "tika_url",
                config_string(),
                "A running Tika server on loopback, e.g. http://127.0.0.1:9998."
            ))
            .setting(setting(
                "tika_jar",
                config_path(),
                "Or a tika-server jar this plugin starts on loopback."
            ))
            .setting(
                setting("java", config_string(), "Java command used with tika_jar.")
                    .default_value(&"java")
            )
            .setting(
                setting(
                    "tika_concurrency",
                    config_integer(),
                    "Concurrent extractions."
                )
                .default_value(&2)
            )
            .setting(setting(
                "root_model",
                config_string(),
                "mesh-docs reading model when the caller named none."
            ))
            .setting(setting(
                "sub_model",
                config_string(),
                "Model for page questions (default: the reading model)."
            )),
    ]
}

fn operations(shared: &Shared) -> OperationRouter {
    let mut router = OperationRouter::new();
    let for_staging = Arc::clone(shared);
    router.add_json::<NoArgs, serde_json::Value, _>(
        json_schema_operation::<NoArgs>(
            "staging_dir",
            "The private directory the host writes PDFs into before prepare_documents.",
        ),
        move |_args, _context| {
            let shared = Arc::clone(&for_staging);
            Box::pin(async move {
                let documents = current(&shared)?;
                Ok(serde_json::json!({ "dir": documents.staging.dir() }))
            })
        },
    );
    let for_prepare = Arc::clone(shared);
    router.add_json::<prepare::PrepareArgs, prepare::PrepareAnswer, _>(
        json_schema_operation::<prepare::PrepareArgs>(
            "prepare_documents",
            "Extract staged PDFs (by file name) and return the text parts for the request.",
        ),
        move |args, _context| {
            let shared = Arc::clone(&for_prepare);
            Box::pin(async move { Ok(current(&shared)?.prepare(args).await) })
        },
    );
    let for_status = Arc::clone(shared);
    router.add_json::<NoArgs, StatusReport, _>(
        json_schema_operation::<NoArgs>("status", "Extraction, cache and mesh-docs counters."),
        move |_args, _context| {
            let shared = Arc::clone(&for_status);
            Box::pin(async move { Ok(current(&shared)?.status()) })
        },
    );
    router
}

fn virtual_models(shared: &Shared) -> VirtualModelRouter {
    let mut router = VirtualModelRouter::new();
    let shared = Arc::clone(shared);
    router.add_raw(
        operation_with_schema(
            HANDLER,
            "Answer from attached documents",
            serde_json::Map::new(),
        ),
        move |request, context| {
            let context: PluginContext<'static> = context.owned();
            let shared = Arc::clone(&shared);
            Box::pin(async move {
                let invocation: VirtualModelInvocation = request.arguments()?;
                let documents = current(&shared)?;
                let infer = rlm::HostInfer(context);
                let response =
                    rlm::run_turn(&documents, &infer, invocation, rlm::Limits::default()).await;
                structured_tool_result(response)
            })
        },
    );
    router
}

pub async fn run() -> anyhow::Result<()> {
    PluginRuntime::run(plugin()).await
}
