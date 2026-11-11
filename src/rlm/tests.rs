use super::*;
use crate::Settings;
use crate::store::document_id;
use crate::test_support::TempDir;
use crate::tokens::Grant;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

const FACT: &str = "AZURE-HERON-42";

type Script = dyn Fn(&HostInferenceRequest) -> Value + Send + Sync;

struct Fake {
    script: Box<Script>,
    requests: Mutex<Vec<HostInferenceRequest>>,
}

impl Fake {
    fn new(script: impl Fn(&HostInferenceRequest) -> Value + Send + Sync + 'static) -> Self {
        Self {
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<HostInferenceRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Infer for Fake {
    async fn infer(&self, request: HostInferenceRequest) -> anyhow::Result<HostInferenceResponse> {
        let body = (self.script)(&request);
        self.requests.lock().unwrap().push(request);
        Ok(HostInferenceResponse {
            status_code: 200,
            body,
            served_by: None,
        })
    }
}

fn answer(content: &str) -> Value {
    json!({"object": "chat.completion", "choices": [{"index": 0, "finish_reason": "stop",
        "message": {"role": "assistant", "content": content}}]})
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({"object": "chat.completion", "choices": [{"index": 0, "finish_reason": "tool_calls",
        "message": {"role": "assistant", "content": null, "tool_calls": [{"id": format!("call-{name}"),
        "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}]})
}

fn candidate(model: &str, tools: bool, context: u32) -> VirtualModelCandidate {
    VirtualModelCandidate {
        model_id: model.into(),
        target_node_id: None,
        parameter_count_b: Some(if model.contains("big") { 70.0 } else { 3.0 }),
        context_length: Some(context),
        deprioritized: false,
        supports_tools: tools,
        supports_vision: false,
        supports_audio: false,
    }
}

/// A 300-page report with the fact on one late page, plus a short appendix.
struct Setup {
    _dir: TempDir,
    documents: Documents,
    token: String,
}

fn setup(caller_model: Option<&str>) -> Setup {
    let dir = TempDir::new();
    let settings = Settings {
        store_dir: Some(dir.path().to_path_buf()),
        ..Settings::default()
    };
    let documents = Documents::new(settings).unwrap();
    let filler = "The committee reviewed routine operating figures for the quarter. ".repeat(40);
    let mut report: Vec<String> = (1..=300)
        .map(|n| format!("Section {n}\n{filler}"))
        .collect();
    report[286] = format!("Section 287\nThe vault access phrase is {FACT}. {filler}");
    let appendix = vec!["Appendix A\nGlossary of terms used in the report.".to_string()];
    let mut ids = Vec::new();
    for (name, pages) in [("report.pdf", report), ("appendix.pdf", appendix)] {
        let pdf = format!("%PDF-1.7 {name}").into_bytes();
        let id = document_id(&pdf);
        documents.store.save(&id, &pdf, name, &pages).unwrap();
        ids.push(id);
    }
    let token = documents.tokens.mint(Grant {
        documents: ids,
        caller_model: caller_model.map(str::to_string),
    });
    Setup {
        _dir: dir,
        documents,
        token,
    }
}

fn invocation(
    setup: &Setup,
    question: &str,
    candidates: Vec<VirtualModelCandidate>,
) -> VirtualModelInvocation {
    let tag = |index: usize, name: &str, pages: usize| {
        crate::text::rlm_part(&setup.token, index, name, pages)
    };
    VirtualModelInvocation {
        request: json!({"model": "mesh-docs", "stream": true, "messages": [{"role": "user", "content": [
            {"type": "text", "text": question},
            {"type": "text", "text": tag(0, "report.pdf", 300)},
            {"type": "text", "text": tag(1, "appendix.pdf", 1)}]}]}),
        candidates,
        response_adapter: String::new(),
    }
}

fn last_tool_result(request: &HostInferenceRequest) -> Option<String> {
    request.request["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .map(str::to_string)
}

/// A root that searches, reads the best page, then answers from it.
fn reader(request: &HostInferenceRequest) -> Value {
    if request.request.get("tools").is_none() {
        return answer("final");
    }
    match last_tool_result(request) {
        None => tool_call("search", json!({"query": "vault access phrase"})),
        Some(result) if result.contains("pages match") => {
            let page: i64 = result
                .split("[page ")
                .nth(1)
                .and_then(|rest| rest.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap();
            tool_call("read", json!({"page_from": page, "page_to": page}))
        }
        Some(result) => answer(&format!(
            "From the page I read: {}",
            &result[..result.len().min(200)]
        )),
    }
}

fn assert_within_budget(fake: &Fake, context: u32) {
    let estimator = Estimator::default();
    let limit = (f64::from(context) * budget::PROMPT_SHARE) as usize;
    for request in fake.requests() {
        let messages = request.request["messages"].as_array().unwrap().clone();
        let tokens = estimator.request_tokens(&messages, request.request.get("tools"));
        assert!(tokens <= limit, "nested prompt {tokens} tokens > {limit}");
        let max_tokens = request.request["max_tokens"].as_u64().unwrap();
        assert!(tokens as u64 + max_tokens < u64::from(context));
        assert_eq!(request.request["stream"], false);
    }
}

#[tokio::test]
async fn the_reading_model_finds_a_fact_on_one_late_page() {
    let setup = setup(Some("small-tools"));
    let fake = Fake::new(reader);
    let context = 8_192;
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(
            &setup,
            "What is the vault access phrase?",
            vec![
                candidate("big-tools", true, context),
                candidate("small-tools", true, context),
            ],
        ),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 200, "{}", response.body);
    assert!(response.event_stream);
    assert_eq!(response.body["model"], "mesh-docs");
    let content = response.body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap();
    assert!(content.contains(FACT), "{content}");
    let requests = fake.requests();
    // The caller's own model was recovered from the token, not the bigger one.
    assert!(requests.iter().all(|r| r.model_id == "small-tools"));
    let system = requests[0].request["messages"][0]["content"]
        .as_str()
        .unwrap();
    assert!(system.contains("Document 1 \"report.pdf\": pages 1–300"));
    assert!(system.contains("Document 2 \"appendix.pdf\": pages 301–301"));
    let user = requests[0].request["messages"][1].to_string();
    assert!(
        !user.contains("mesh-document"),
        "tags are described, not passed on"
    );
    assert_within_budget(&fake, context);
    let report = setup
        .documents
        .stats
        .last_turn
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!(report.steps, 3);
    assert!(report.max_tool_result_chars <= 8_000);
    assert!(!report.partial);
}

#[tokio::test]
async fn tool_results_are_dropped_oldest_first_to_stay_in_budget() {
    let setup = setup(None);
    // Reads ten pages per step for twelve steps on a small context.
    let fake = Fake::new(|request| {
        if request.request.get("tools").is_none() {
            return answer("summary");
        }
        tool_call("read", json!({"page_from": 1, "page_to": 10}))
    });
    let context = 4_096;
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(
            &setup,
            "Summarise everything",
            vec![candidate("small-tools", true, context)],
        ),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 200, "{}", response.body);
    let content = response.body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap();
    assert!(content.starts_with(PARTIAL_NOTE), "{content}");
    let report = setup
        .documents
        .stats
        .last_turn
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!(report.steps, 12);
    assert!(report.partial);
    assert!(report.dropped_tool_results > 0);
    assert_within_budget(&fake, context);
    let last = fake.requests().pop().unwrap();
    assert!(
        last.request.get("tools").is_none(),
        "the final answer is asked without tools"
    );
}

#[tokio::test]
async fn asks_go_to_the_sub_model_with_bounded_pages() {
    let setup = setup(None);
    let fake = Fake::new(|request| {
        if request.model_id == "helper" {
            return answer(&format!("The phrase is {FACT} (page 287)."));
        }
        match last_tool_result(request) {
            None => tool_call(
                "ask",
                json!({"question": "What is the vault phrase?", "page_from": 250, "page_to": 300}),
            ),
            Some(result) => answer(&result),
        }
    });
    let mut documents = setup.documents;
    documents.settings.sub_model = Some("helper".into());
    let setup = Setup { documents, ..setup };
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(
            &setup,
            "What is the vault phrase?",
            vec![
                candidate("big-tools", true, 8_192),
                candidate("helper", false, 4_096),
            ],
        ),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 200, "{}", response.body);
    let content = response.body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap();
    assert!(
        content.contains("Helper's answer from pages 250–300 (range cut to fit)"),
        "{content}"
    );
    let helper: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|r| r.model_id == "helper")
        .collect();
    assert_eq!(helper.len(), 1);
    let estimator = Estimator::default();
    let messages = helper[0].request["messages"].as_array().unwrap().clone();
    assert!(estimator.request_tokens(&messages, None) <= (4_096.0 * budget::PROMPT_SHARE) as usize);
}

#[tokio::test]
async fn without_a_tool_capable_model_the_best_pages_go_in_one_prompt() {
    let setup = setup(None);
    let fake = Fake::new(|_| answer("It is in the excerpt."));
    let context = 8_192;
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(
            &setup,
            "What is the vault access phrase?",
            vec![candidate("plain", false, context)],
        ),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 200, "{}", response.body);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].request.get("tools").is_none());
    let system = requests[0].request["messages"][0]["content"]
        .as_str()
        .unwrap();
    assert!(system.contains(FACT));
    let content = response.body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap();
    assert!(
        content.starts_with("Note: I answered from the "),
        "{content}"
    );
    assert_within_budget(&fake, context);
}

#[tokio::test]
async fn missing_unknown_and_expired_tokens() {
    let setup = setup(None);
    let fake = Fake::new(|_| answer("x"));
    let candidates = vec![candidate("small-tools", true, 8_192)];
    let plain = VirtualModelInvocation {
        request: json!({"messages": [{"role": "user", "content": "hi"}]}),
        candidates: candidates.clone(),
        response_adapter: String::new(),
    };
    let response = run_turn(&setup.documents, &fake, plain, Limits::default()).await;
    assert_eq!(response.status_code, 400);
    assert_eq!(response.body["error"]["code"], "document_token_missing");

    let mut unknown = invocation(&setup, "q", candidates.clone());
    let text = unknown
        .request
        .to_string()
        .replace(&setup.token, &"0".repeat(32));
    unknown.request = serde_json::from_str(&text).unwrap();
    let response = run_turn(&setup.documents, &fake, unknown, Limits::default()).await;
    assert_eq!(response.status_code, 400);

    // A token whose documents were swept is expired.
    setup.documents.store.sweep_all_for_test();
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(&setup, "q", candidates),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 410);
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn a_conversation_too_long_for_the_model_is_413() {
    let setup = setup(None);
    let fake = Fake::new(|_| answer("x"));
    let question = "word ".repeat(20_000);
    let response = run_turn(
        &setup.documents,
        &fake,
        invocation(
            &setup,
            &question,
            vec![candidate("small-tools", true, 4_096)],
        ),
        Limits::default(),
    )
    .await;
    assert_eq!(response.status_code, 413);
    assert!(fake.requests().is_empty());
}

/// A model that never answers. The plugin's own deadline must end the turn
/// and drop every nested call, since no client-disconnect signal exists.
struct Hung {
    started: AtomicUsize,
    in_flight: Arc<AtomicUsize>,
    timeouts: Mutex<Vec<u64>>,
}

struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Infer for Hung {
    async fn infer(&self, request: HostInferenceRequest) -> anyhow::Result<HostInferenceResponse> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.timeouts
            .lock()
            .unwrap()
            .push(request.timeout_ms.unwrap_or(u64::MAX));
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _guard = InFlight(Arc::clone(&self.in_flight));
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn the_240_second_deadline_stops_nested_work() {
    let setup = setup(None);
    let hung = Hung {
        started: AtomicUsize::new(0),
        in_flight: Arc::new(AtomicUsize::new(0)),
        timeouts: Mutex::new(Vec::new()),
    };
    let started = Instant::now();
    let response = run_turn(
        &setup.documents,
        &hung,
        invocation(
            &setup,
            "What is the vault phrase?",
            vec![candidate("small-tools", true, 8_192)],
        ),
        Limits::default(),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.status_code, 504, "{}", response.body);
    assert!(elapsed <= Duration::from_secs(240), "{elapsed:?}");
    assert!(elapsed >= Duration::from_secs(200), "{elapsed:?}");
    // The loop call (cut at 200 s) and the final answer call (to 240 s).
    assert_eq!(hung.started.load(Ordering::SeqCst), 2);
    assert_eq!(
        hung.in_flight.load(Ordering::SeqCst),
        0,
        "nested calls were dropped"
    );
    assert!(
        hung.timeouts
            .lock()
            .unwrap()
            .iter()
            .all(|ms| *ms <= 240_000)
    );
}

#[test]
fn model_choice_prefers_the_caller_then_settings_then_the_best_tool_model() {
    let candidates = vec![
        candidate("small-plain", false, 8_192),
        candidate("big-tools", true, 8_192),
        candidate("small-tools", true, 8_192),
    ];
    let pick = |caller, root, sub| {
        let models = choose_models(&candidates, caller, root, sub).unwrap();
        (models.root.model_id, models.sub.model_id)
    };
    assert_eq!(
        pick(Some("small-plain"), None, None),
        ("small-plain".into(), "small-plain".into())
    );
    assert_eq!(
        pick(Some("auto"), Some("small-tools"), None),
        ("small-tools".into(), "small-tools".into())
    );
    assert_eq!(
        pick(Some("gone"), None, Some("small-plain")),
        ("big-tools".into(), "small-plain".into())
    );
    assert!(choose_models(&[], None, None, None).is_none());
}
