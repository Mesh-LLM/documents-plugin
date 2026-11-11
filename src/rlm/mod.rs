//! `mesh-docs`: answer from documents too large for the caller's model.
//!
//! A root model reads the documents through three fixed tools (`search`,
//! `read`, `ask`); no model-written code runs. Every nested prompt, tool
//! schemas included, stays within 60% of that model's context in estimated
//! tokens, dropping the oldest tool results first. The turn is bounded by
//! step, size, concurrency and a 240 s deadline that the plugin enforces
//! itself: every nested call carries the remaining time and is dropped when
//! it runs out, because the host does not yet report client disconnects.

mod budget;
mod corpus;
#[cfg(test)]
mod tests;

use crate::Documents;
use crate::tokens::Lookup;
use budget::{Budget, Estimator};
use corpus::Corpus;
use futures::future::join_all;
use mesh_llm_plugin::{
    HostInferenceRequest, HostInferenceResponse, PluginContext, VirtualModelCandidate,
    VirtualModelInvocation, VirtualModelResponse,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_steps: usize,
    pub max_tool_chars: usize,
    pub max_concurrent_asks: usize,
    pub total: Duration,
    /// Kept back from the tool loop so a final answer can still be written.
    pub final_reserve: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 12,
            max_tool_chars: 8_000,
            max_concurrent_asks: 4,
            total: Duration::from_secs(240),
            final_reserve: Duration::from_secs(40),
        }
    }
}

#[async_trait::async_trait]
pub trait Infer: Send + Sync {
    async fn infer(&self, request: HostInferenceRequest) -> anyhow::Result<HostInferenceResponse>;
}

pub struct HostInfer(pub PluginContext<'static>);

#[async_trait::async_trait]
impl Infer for HostInfer {
    async fn infer(&self, request: HostInferenceRequest) -> anyhow::Result<HostInferenceResponse> {
        self.0.infer(request).await
    }
}

/// What one turn did, for the `status` operation and tests.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TurnReport {
    /// The models the host offered, as `id (tools, context)`.
    pub candidates: Vec<String>,
    pub caller_model: Option<String>,
    pub root_model: String,
    pub sub_model: String,
    pub reading: &'static str,
    pub steps: usize,
    pub nested_calls: usize,
    pub tool_calls: Vec<String>,
    pub prompt_budget_tokens: usize,
    pub max_estimated_prompt_tokens: usize,
    pub max_reported_prompt_tokens: u64,
    pub max_tool_result_chars: usize,
    pub dropped_tool_results: usize,
    pub partial: bool,
}

pub async fn run_turn(
    documents: &Documents,
    infer: &dyn Infer,
    invocation: VirtualModelInvocation,
    limits: Limits,
) -> VirtualModelResponse {
    documents.stats.rlm_turns.fetch_add(1, Ordering::Relaxed);
    let stream = invocation
        .request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let deadline = Instant::now() + limits.total;
    let report = Mutex::new(TurnReport::default());
    let turn = Turn {
        documents,
        infer,
        limits,
        deadline,
        estimator: Estimator::default(),
        report: &report,
        asks: Semaphore::new(limits.max_concurrent_asks.max(1)),
    };
    let result = tokio::time::timeout_at(deadline, turn.run(invocation))
        .await
        .unwrap_or_else(|_| Err(timed_out()));
    *documents
        .stats
        .last_turn
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        Some(report.into_inner().unwrap_or_else(|e| e.into_inner()));
    match result {
        Ok(mut body) => {
            if body.is_object() {
                body["model"] = Value::String(crate::VIRTUAL_MODEL.into());
            }
            VirtualModelResponse {
                status_code: 200,
                body,
                headers: Vec::new(),
                event_stream: stream,
            }
        }
        Err(response) => response,
    }
}

fn error(status: u16, code: &str, message: impl Into<String>) -> VirtualModelResponse {
    VirtualModelResponse {
        status_code: status,
        body: json!({"error": {"message": message.into(), "type": "document_error", "code": code}}),
        headers: Vec::new(),
        event_stream: false,
    }
}

fn timed_out() -> VirtualModelResponse {
    error(
        504,
        "document_timeout",
        "reading the document took longer than the mesh-docs time limit",
    )
}

type TurnResult<T> = Result<T, VirtualModelResponse>;

struct Turn<'a> {
    documents: &'a Documents,
    infer: &'a dyn Infer,
    limits: Limits,
    deadline: Instant,
    estimator: Estimator,
    report: &'a Mutex<TurnReport>,
    asks: Semaphore,
}

struct Models {
    root: VirtualModelCandidate,
    sub: VirtualModelCandidate,
}

const TOKEN_TAG: &str = "<mesh-document token=\"";

/// The newest token in the conversation: the host mints one per request.
fn find_token(messages: &[Value]) -> Option<String> {
    let mut found = None;
    for message in messages {
        for text in message_texts(message) {
            let mut rest = text;
            while let Some(at) = rest.find(TOKEN_TAG) {
                let after = &rest[at + TOKEN_TAG.len()..];
                if let Some(token) = after
                    .get(..32)
                    .filter(|t| t.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    found = Some(token.to_string());
                }
                rest = after;
            }
        }
    }
    found
}

fn message_texts(message: &Value) -> Vec<&str> {
    match message.get("content") {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Replace each `<mesh-document …/>` tag with a short description.
fn describe_tags(text: &str, corpus: &Corpus) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("<mesh-document ") {
        out.push_str(&rest[..at]);
        let Some(end) = rest[at..].find("/>") else {
            out.push_str(&rest[at..]);
            return out;
        };
        let tag = &rest[at..at + end];
        let index = tag
            .split("index=\"")
            .nth(1)
            .and_then(|v| v.split('"').next())
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        match corpus.documents.get(index) {
            Some(doc) => out.push_str(&format!(
                "[Attached document {}: \"{}\", pages {}–{}. It is too long to show here; read it with the tools.]",
                index + 1,
                crate::text::escape_attr(&doc.filename),
                doc.first_page,
                doc.first_page + doc.pages.saturating_sub(1)
            )),
            None => out.push_str("[Attached document]"),
        }
        rest = &rest[at + end + 2..];
    }
    out.push_str(rest);
    out
}

fn conversation(messages: &[Value], corpus: &Corpus) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            match message.get_mut("content") {
                Some(Value::String(text)) => *text = describe_tags(text, corpus),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(Value::String(text)) = part.get_mut("text") {
                            *text = describe_tags(text, corpus);
                        }
                    }
                }
                _ => {}
            }
            message
        })
        .collect()
}

fn last_user_text(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|message| message_texts(message).join("\n"))
        .unwrap_or_default()
}

fn is_concrete(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && !matches!(model, "auto" | "mesh" | crate::VIRTUAL_MODEL)
}

fn rank(left: &VirtualModelCandidate, right: &VirtualModelCandidate) -> std::cmp::Ordering {
    left.deprioritized
        .cmp(&right.deprioritized)
        .then(right.supports_tools.cmp(&left.supports_tools))
        .then(
            right
                .parameter_count_b
                .unwrap_or(0.0)
                .total_cmp(&left.parameter_count_b.unwrap_or(0.0)),
        )
        .then(right.context_length.cmp(&left.context_length))
        .then(left.model_id.cmp(&right.model_id))
}

/// Root: the caller's own model, else `root_model`, else the best
/// tool-capable candidate. Sub: `sub_model`, else the root.
fn choose_models(
    candidates: &[VirtualModelCandidate],
    caller_model: Option<&str>,
    root_setting: Option<&str>,
    sub_setting: Option<&str>,
) -> Option<Models> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by(rank);
    let named = |name: Option<&str>| {
        name.filter(|name| is_concrete(name))
            .and_then(|name| sorted.iter().find(|c| c.model_id == name))
            .cloned()
    };
    let root = named(caller_model)
        .or_else(|| named(root_setting))
        .or_else(|| sorted.first().cloned())?;
    let sub = named(sub_setting).unwrap_or_else(|| root.clone());
    Some(Models { root, sub })
}

const ROOT_PROMPT: &str = "You answer questions about attached documents that are too long to show you in full. \
Use the tools to look inside them: search(query) lists pages that mention words, read(page_from, page_to) returns \
page text, and ask(question, page_from, page_to) has a helper read a page range and answer one question (use it to \
cover many pages at once). Pages are numbered across all documents as in the outline below. Look before you answer \
and cite page numbers. If what you read does not answer the question, say so plainly; do not guess.";

const RETRIEVAL_PROMPT: &str = "You answer questions about attached documents. Below are the pages that best match \
the question; they are excerpts, not the whole document. Answer only from them and cite page numbers. If they do not \
answer the question, say so plainly; do not guess.";

const ASK_PROMPT: &str = "Answer the question using only the document pages below, citing page numbers. If the pages \
do not contain the answer, reply exactly: NOT IN THESE PAGES.";

const FINAL_NUDGE: &str = "The reading limit is reached. Do not call tools. Answer now from what you have read, and \
say clearly if it does not settle the question.";

const PARTIAL_NOTE: &str =
    "Note: I reached the reading limit, so only part of the document was read.\n\n";

fn tools() -> Value {
    let range = json!({
        "page_from": {"type": "integer", "description": "First page, 1-based"},
        "page_to": {"type": "integer", "description": "Last page, inclusive"}
    });
    let mut ask_properties = range.as_object().cloned().unwrap_or_default();
    ask_properties.insert("question".into(), json!({"type": "string"}));
    json!([
        {"type": "function", "function": {
            "name": "search", "description": "Find pages that mention these words.",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}},
        {"type": "function", "function": {
            "name": "read", "description": "Read the text of a page range.",
            "parameters": {"type": "object", "properties": range, "required": ["page_from", "page_to"]}}},
        {"type": "function", "function": {
            "name": "ask", "description": "Have a helper read a page range and answer one question about it.",
            "parameters": {"type": "object", "properties": ask_properties, "required": ["question", "page_from", "page_to"]}}}
    ])
}

fn copy_sampling(from: &Value, into: &mut Value) {
    for key in ["temperature", "top_p", "seed"] {
        if let Some(value) = from.get(key) {
            into[key] = value.clone();
        }
    }
}

fn wanted_output(request: &Value) -> Option<u64> {
    request
        .get("max_completion_tokens")
        .or_else(|| request.get("max_tokens"))
        .and_then(Value::as_u64)
}

fn first_message(body: &Value) -> Option<&Value> {
    body.get("choices")?.get(0)?.get("message")
}

impl Turn<'_> {
    fn note(&self, update: impl FnOnce(&mut TurnReport)) {
        update(&mut self.report.lock().unwrap_or_else(|e| e.into_inner()));
    }

    async fn run(&self, invocation: VirtualModelInvocation) -> TurnResult<Value> {
        let request = invocation.request;
        let messages = request
            .get("messages")
            .and_then(Value::as_array)
            .filter(|messages| !messages.is_empty())
            .ok_or_else(|| {
                error(
                    400,
                    "invalid_request",
                    "mesh-docs needs a non-empty messages array",
                )
            })?;
        let token = find_token(messages).ok_or_else(|| {
            error(
                400,
                "document_token_missing",
                "mesh-docs answers about PDFs attached to the request; none was found",
            )
        })?;
        let grant = match self.documents.tokens.lookup(&token) {
            Lookup::Valid(grant) => grant,
            Lookup::Expired => {
                return Err(error(
                    410,
                    "document_expired",
                    "the attached document expired; send it again",
                ));
            }
            Lookup::Unknown => {
                return Err(error(
                    400,
                    "document_token_unknown",
                    "the attached document is not known here; send it again",
                ));
            }
        };
        let _pin = self.documents.store.pin(&grant.documents);
        let mut loaded = Vec::new();
        for id in &grant.documents {
            let meta = self.documents.store.lookup(id);
            let pages = self.documents.store.pages(id);
            match (meta, pages) {
                (Some(entry), Some(pages)) => loaded.push((entry.meta.filename, pages)),
                _ => {
                    return Err(error(
                        410,
                        "document_expired",
                        "the attached document expired; send it again",
                    ));
                }
            }
        }
        let corpus = Corpus::new(loaded);
        let settings = &self.documents.settings;
        let models = choose_models(
            &invocation.candidates,
            grant.caller_model.as_deref(),
            settings.root_model.as_deref(),
            settings.sub_model.as_deref(),
        )
        .ok_or_else(|| {
            error(
                503,
                "no_model_available",
                "no model is available to read the document",
            )
        })?;
        self.note(|r| {
            r.candidates = invocation
                .candidates
                .iter()
                .map(|c| {
                    let tools = if c.supports_tools { "tools" } else { "no tools" };
                    let context = c.context_length.map_or("?".into(), |n| n.to_string());
                    format!("{} ({tools}, {context})", c.model_id)
                })
                .collect();
            r.caller_model = grant.caller_model.clone();
            r.root_model = models.root.model_id.clone();
            r.sub_model = models.sub.model_id.clone();
        });
        let conversation = conversation(messages, &corpus);
        if models.root.supports_tools {
            self.tool_loop(&request, conversation, &corpus, &models)
                .await
        } else {
            self.retrieval(&request, conversation, &corpus, &models.root)
                .await
        }
    }

    /// One nested chat call under the remaining time.
    async fn call(
        &self,
        model: &VirtualModelCandidate,
        body: Value,
        until: Instant,
    ) -> TurnResult<Value> {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let bytes = budget::request_bytes(&messages, body.get("tools"));
        let estimated = self.estimator.request_tokens(&messages, body.get("tools"));
        self.note(|r| {
            r.nested_calls += 1;
            r.max_estimated_prompt_tokens = r.max_estimated_prompt_tokens.max(estimated);
        });
        let request = HostInferenceRequest {
            model_id: model.model_id.clone(),
            target_node_id: model.target_node_id.clone(),
            request: body,
            timeout_ms: Some(remaining.as_millis().min(u128::from(u64::MAX)) as u64),
        };
        let response = tokio::time::timeout_at(until, self.infer.infer(request))
            .await
            .map_err(|_| timed_out())?
            .map_err(|e| {
                error(
                    502,
                    "document_model_failed",
                    format!("reading model failed: {e}"),
                )
            })?;
        if !(200..300).contains(&response.status_code) {
            return Err(VirtualModelResponse {
                status_code: response.status_code,
                body: response.body,
                headers: Vec::new(),
                event_stream: false,
            });
        }
        if let Some(prompt) = response
            .body
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
        {
            self.estimator.observe(bytes, prompt);
            self.note(|r| r.max_reported_prompt_tokens = r.max_reported_prompt_tokens.max(prompt));
        }
        Ok(response.body)
    }

    fn system_message(&self, prompt: &str, corpus: &Corpus, budget: Budget) -> Value {
        // The outline gets at most a fifth of the prompt budget.
        let outline_bytes = self.estimator.bytes_for_tokens(budget.prompt_tokens() / 5);
        json!({"role": "system", "content": format!("{prompt}\n\n{}", corpus.outline(outline_bytes))})
    }

    async fn tool_loop(
        &self,
        request: &Value,
        conversation: Vec<Value>,
        corpus: &Corpus,
        models: &Models,
    ) -> TurnResult<Value> {
        let budget = Budget::new(models.root.context_length);
        let tools = tools();
        let mut messages = vec![self.system_message(ROOT_PROMPT, corpus, budget)];
        messages.extend(conversation);
        let base = messages.len();
        self.note(|r| {
            r.reading = "tools";
            r.prompt_budget_tokens = budget.prompt_tokens();
        });
        if self.estimator.request_tokens(&messages, Some(&tools)) > budget.prompt_tokens() {
            return Err(error(
                413,
                "conversation_too_long",
                "the conversation alone is too long for the reading model",
            ));
        }
        let loop_until = self
            .deadline
            .checked_sub(self.limits.final_reserve)
            .unwrap_or(self.deadline);
        let max_tokens = budget.output_tokens(wanted_output(request));
        for _ in 0..self.limits.max_steps {
            if Instant::now() >= loop_until {
                break;
            }
            self.fit(&mut messages, base, Some(&tools), budget);
            let mut body = json!({"model": models.root.model_id, "messages": messages, "tools": tools,
                "tool_choice": "auto", "max_tokens": max_tokens, "stream": false});
            copy_sampling(request, &mut body);
            let answer = self.call(&models.root, body, loop_until).await;
            let answer = match answer {
                Err(response) if response.status_code == 504 && Instant::now() < self.deadline => {
                    break;
                }
                other => other?,
            };
            self.note(|r| r.steps += 1);
            let message = first_message(&answer).cloned().unwrap_or_else(|| json!({}));
            let calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                return Ok(answer);
            }
            messages.push(json!({"role": "assistant", "content": message.get("content").cloned().unwrap_or(Value::Null), "tool_calls": calls}));
            let results = join_all(
                calls
                    .iter()
                    .map(|call| self.run_tool(call, corpus, &models.sub)),
            )
            .await;
            for (call, result) in calls.iter().zip(results) {
                let id = call.get("id").cloned().unwrap_or(Value::Null);
                messages.push(json!({"role": "tool", "tool_call_id": id, "content": result}));
            }
        }
        self.finish_partial(request, messages, base, models, budget, max_tokens)
            .await
    }

    async fn finish_partial(
        &self,
        request: &Value,
        mut messages: Vec<Value>,
        base: usize,
        models: &Models,
        budget: Budget,
        max_tokens: u64,
    ) -> TurnResult<Value> {
        self.note(|r| r.partial = true);
        messages.push(json!({"role": "user", "content": FINAL_NUDGE}));
        self.fit(&mut messages, base, None, budget);
        let mut body = json!({"model": models.root.model_id, "messages": messages,
            "max_tokens": max_tokens, "stream": false});
        copy_sampling(request, &mut body);
        let mut answer = self.call(&models.root, body, self.deadline).await?;
        prefix_answer(&mut answer, PARTIAL_NOTE);
        Ok(answer)
    }

    /// Drop the oldest tool results until the prompt fits.
    fn fit(&self, messages: &mut [Value], base: usize, tools: Option<&Value>, budget: Budget) {
        let limit = budget.prompt_tokens();
        let mut next = base;
        while self.estimator.request_tokens(messages, tools) > limit && next < messages.len() {
            let message = &mut messages[next];
            next += 1;
            if message.get("role").and_then(Value::as_str) == Some("tool")
                && message.get("content").and_then(Value::as_str) != Some(DROPPED)
            {
                message["content"] = Value::String(DROPPED.into());
                self.note(|r| r.dropped_tool_results += 1);
            }
        }
    }

    async fn run_tool(&self, call: &Value, corpus: &Corpus, sub: &VirtualModelCandidate) -> String {
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arguments: Value = call
            .pointer("/function/arguments")
            .and_then(|args| match args {
                Value::String(text) => serde_json::from_str(text).ok(),
                other => Some(other.clone()),
            })
            .unwrap_or_else(|| json!({}));
        let int = |key: &str| arguments.get(key).and_then(Value::as_i64);
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        self.note(|r| r.tool_calls.push(format!("{name}({arguments})")));
        let max = self.limits.max_tool_chars;
        let result = match (name, int("page_from"), int("page_to")) {
            ("search", _, _) => corpus.search(&text("query"), max),
            ("read", Some(from), Some(to)) => corpus.read(from, to, max),
            ("ask", Some(from), Some(to)) => {
                self.ask(&text("question"), from, to, corpus, sub).await
            }
            ("read" | "ask", _, _) => "error: page_from and page_to are required integers".into(),
            _ => format!("error: unknown tool {name:?}; use search, read or ask"),
        };
        let result = corpus::truncate_chars(&result, max, "[cut]");
        let chars = result.chars().count();
        self.note(|r| r.max_tool_result_chars = r.max_tool_result_chars.max(chars));
        result
    }

    async fn ask(
        &self,
        question: &str,
        from: i64,
        to: i64,
        corpus: &Corpus,
        sub: &VirtualModelCandidate,
    ) -> String {
        let Some((from, to)) = corpus.range(from, to) else {
            return "The documents have no pages.".into();
        };
        let Ok(_permit) = self.asks.acquire().await else {
            return "error: ask is unavailable".into();
        };
        let budget = Budget::new(sub.context_length);
        let question_message = json!({"role": "user", "content": format!("Question: {question}")});
        let pages = corpus.pages_text(from, to);
        let build = |text: &str| {
            let cut = if text.len() < pages.len() {
                "\n[the rest of the range was cut to fit]"
            } else {
                ""
            };
            vec![
                json!({"role": "system", "content": ASK_PROMPT}),
                json!({"role": "user", "content": format!("<pages>\n{text}{cut}\n</pages>")}),
                question_message.clone(),
            ]
        };
        let fitted = self.fit_text(&pages, budget.prompt_tokens(), None, build);
        let cut = fitted.len() < pages.len();
        let body = json!({"model": sub.model_id, "stream": false, "max_tokens": budget.output_tokens(Some(512)),
            "messages": build(fitted)});
        match self.call(sub, body, self.deadline).await {
            Ok(answer) => {
                let content = first_message(&answer)
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("(no answer)");
                format!(
                    "Helper's answer from pages {from}–{to}{}: {content}",
                    if cut { " (range cut to fit)" } else { "" }
                )
            }
            Err(response) => format!("error: the helper failed (HTTP {})", response.status_code),
        }
    }

    /// The longest prefix of `text` whose request, as `build` makes it, fits
    /// `limit` estimated tokens (JSON escaping included).
    fn fit_text<'t>(
        &self,
        text: &'t str,
        limit: usize,
        tools: Option<&Value>,
        build: impl Fn(&str) -> Vec<Value>,
    ) -> &'t str {
        let mut fitted = text;
        loop {
            let tokens = self.estimator.request_tokens(&build(fitted), tools);
            if tokens <= limit || fitted.is_empty() {
                return fitted;
            }
            let over = self.estimator.bytes_for_tokens(tokens - limit).max(64);
            fitted = corpus::truncate_bytes(fitted, fitted.len().saturating_sub(over + over / 4));
        }
    }

    /// No tool-capable model: the best-matching pages in one prompt.
    async fn retrieval(
        &self,
        request: &Value,
        conversation: Vec<Value>,
        corpus: &Corpus,
        root: &VirtualModelCandidate,
    ) -> TurnResult<Value> {
        let budget = Budget::new(root.context_length);
        self.note(|r| {
            r.reading = "retrieval";
            r.prompt_budget_tokens = budget.prompt_tokens();
        });
        let mut messages = vec![json!({"role": "system", "content": RETRIEVAL_PROMPT})];
        messages.extend(conversation);
        let used = self.estimator.request_tokens(&messages, None);
        if used > budget.prompt_tokens() {
            return Err(error(
                413,
                "conversation_too_long",
                "the conversation alone is too long for the reading model",
            ));
        }
        let mut room = self
            .estimator
            .bytes_for_tokens(budget.prompt_tokens() - used)
            .saturating_sub(256);
        let mut ranked: Vec<usize> = corpus
            .rank(&last_user_text(&messages))
            .into_iter()
            .map(|(page, _)| page)
            .collect();
        if ranked.is_empty() {
            ranked = (1..=corpus.page_count()).collect();
        }
        let mut chosen = Vec::new();
        for page in ranked {
            let len = corpus.pages_text(page, page).len() + 4;
            if len <= room {
                room -= len;
                chosen.push(page);
            }
        }
        chosen.sort_unstable();
        let excerpts: Vec<String> = chosen
            .iter()
            .map(|page| corpus.pages_text(*page, *page))
            .collect();
        let joined = excerpts.join("\n\n");
        let conversation_tail = messages[1..].to_vec();
        let build = |text: &str| {
            let mut all = vec![
                json!({"role": "system", "content": format!("{RETRIEVAL_PROMPT}\n\n<excerpts>\n{text}\n</excerpts>")}),
            ];
            all.extend(conversation_tail.iter().cloned());
            all
        };
        let fitted = self.fit_text(&joined, budget.prompt_tokens(), None, build);
        let messages = build(fitted);
        self.note(|r| r.partial = chosen.len() < corpus.page_count());
        let mut body = json!({"model": root.model_id, "messages": messages,
            "max_tokens": budget.output_tokens(wanted_output(request)), "stream": false});
        copy_sampling(request, &mut body);
        let mut answer = self.call(root, body, self.deadline).await?;
        if chosen.len() < corpus.page_count() {
            prefix_answer(
                &mut answer,
                &format!(
                    "Note: I answered from the {} of {} pages that best matched the question, not the whole document.\n\n",
                    chosen.len(),
                    corpus.page_count()
                ),
            );
        }
        Ok(answer)
    }
}

const DROPPED: &str = "[result dropped to fit the context; call the tool again if you need it]";

fn prefix_answer(answer: &mut Value, note: &str) {
    if let Some(content) = answer.pointer_mut("/choices/0/message/content") {
        let text = content.as_str().unwrap_or_default();
        *content = Value::String(format!("{note}{text}"));
    }
}
