//! Prompt budgets in tokens.
//!
//! The plugin has no tokenizer for the models it calls, so it estimates
//! conservatively from the serialized request (messages *and* tool schemas):
//! one token per three bytes plus a per-message template allowance. Each
//! nested answer's reported `usage.prompt_tokens` then tightens the ratio for
//! the rest of the turn if the model's tokenizer turned out denser. The ratio
//! only ever tightens.

use serde_json::Value;
use std::sync::Mutex;

const INITIAL_BYTES_PER_TOKEN: f64 = 3.0;
const MIN_BYTES_PER_TOKEN: f64 = 1.0;
const PER_MESSAGE_TOKENS: usize = 8;
const DEFAULT_CONTEXT: u32 = 8_192;

/// Prompt (messages plus tools) stays within this share of the context.
pub const PROMPT_SHARE: f64 = 0.6;
/// The answer gets at most this share; the rest is margin.
const OUTPUT_SHARE: f64 = 0.3;

pub struct Estimator {
    bytes_per_token: Mutex<f64>,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            bytes_per_token: Mutex::new(INITIAL_BYTES_PER_TOKEN),
        }
    }
}

impl Estimator {
    fn ratio(&self) -> f64 {
        *self
            .bytes_per_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn tokens_for_bytes(&self, bytes: usize) -> usize {
        (bytes as f64 / self.ratio()).ceil() as usize
    }

    pub fn bytes_for_tokens(&self, tokens: usize) -> usize {
        (tokens as f64 * self.ratio()).floor() as usize
    }

    /// Estimated prompt tokens of a chat request: messages and tool schemas.
    pub fn request_tokens(&self, messages: &[Value], tools: Option<&Value>) -> usize {
        let bytes = request_bytes(messages, tools);
        self.tokens_for_bytes(bytes) + PER_MESSAGE_TOKENS * messages.len()
    }

    /// Learn from a nested call's reported prompt size.
    pub fn observe(&self, request_bytes: usize, prompt_tokens: u64) {
        if prompt_tokens == 0 {
            return;
        }
        let observed = (request_bytes as f64 / prompt_tokens as f64).max(MIN_BYTES_PER_TOKEN);
        let mut ratio = self
            .bytes_per_token
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if observed < *ratio {
            *ratio = observed;
        }
    }
}

pub fn request_bytes(messages: &[Value], tools: Option<&Value>) -> usize {
    messages
        .iter()
        .map(|message| message.to_string().len())
        .sum::<usize>()
        + tools.map_or(0, |tools| tools.to_string().len())
}

#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub context: u32,
}

impl Budget {
    pub fn new(context_length: Option<u32>) -> Self {
        Self {
            context: context_length.filter(|n| *n > 0).unwrap_or(DEFAULT_CONTEXT),
        }
    }

    pub fn prompt_tokens(self) -> usize {
        (f64::from(self.context) * PROMPT_SHARE) as usize
    }

    /// `max_tokens` for a nested call: the caller's wish within our share.
    pub fn output_tokens(self, wanted: Option<u64>) -> u64 {
        let cap = (f64::from(self.context) * OUTPUT_SHARE) as u64;
        wanted.unwrap_or(1_024).clamp(1, cap.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_and_output_shares_leave_room() {
        let budget = Budget::new(Some(10_000));
        assert_eq!(budget.prompt_tokens(), 6_000);
        assert_eq!(budget.output_tokens(None), 1_024);
        assert_eq!(budget.output_tokens(Some(50_000)), 3_000);
        assert!(budget.prompt_tokens() as u64 + budget.output_tokens(Some(50_000)) < 10_000);
        assert_eq!(Budget::new(None).context, 8_192);
    }

    #[test]
    fn tool_schemas_count_and_denser_tokenizers_tighten_the_estimate() {
        let estimator = Estimator::default();
        let messages = vec![json!({"role": "user", "content": "x".repeat(300)})];
        let tools = json!([{"type": "function", "function": {"name": "t", "description": "y".repeat(300)}}]);
        let without = estimator.request_tokens(&messages, None);
        let with = estimator.request_tokens(&messages, Some(&tools));
        assert!(with >= without + 100);
        let bytes = request_bytes(&messages, Some(&tools));
        estimator.observe(bytes, (bytes / 2) as u64);
        assert!(estimator.request_tokens(&messages, Some(&tools)) >= bytes / 2);
        // A sparser tokenizer never loosens it.
        estimator.observe(bytes, (bytes / 10) as u64);
        assert!(estimator.request_tokens(&messages, Some(&tools)) >= bytes / 2);
    }
}
