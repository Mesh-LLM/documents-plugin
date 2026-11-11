//! Tokens linking a rewritten request to its documents for `mesh-docs`.
//!
//! A token is 128 random bits, valid for ten minutes and usable more than once
//! (host retries replay the same rewritten body). It records the documents
//! and the model the caller asked for. It never leaves this node: the client
//! keeps its original PDF part, so each later turn mints a new token, and the
//! extraction cache makes that cheap.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

pub const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
/// Expired tokens are remembered this long so callers get 410, not 400.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug)]
pub struct Grant {
    pub documents: Vec<String>,
    pub caller_model: Option<String>,
}

pub enum Lookup {
    Valid(Grant),
    Expired,
    Unknown,
}

#[derive(Default)]
pub struct Tokens {
    grants: Mutex<HashMap<String, (Instant, Grant)>>,
}

impl Tokens {
    pub fn mint(&self, grant: Grant) -> String {
        let token = hex::encode(rand::random::<[u8; 16]>());
        let mut grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        grants.retain(|_, (minted, _)| now.duration_since(*minted) < FORGET_AFTER);
        grants.insert(token.clone(), (now, grant));
        token
    }

    pub fn lookup(&self, token: &str) -> Lookup {
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        match grants.get(token) {
            Some((minted, grant)) if minted.elapsed() < TOKEN_TTL => Lookup::Valid(grant.clone()),
            Some(_) => Lookup::Expired,
            None => Lookup::Unknown,
        }
    }

    pub fn len(&self) -> usize {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn tokens_are_reusable_until_they_expire() {
        let tokens = Tokens::default();
        let token = tokens.mint(Grant {
            documents: vec!["a".into()],
            caller_model: Some("m".into()),
        });
        assert_eq!(token.len(), 32);
        assert!(matches!(tokens.lookup(&token), Lookup::Valid(_)));
        assert!(matches!(tokens.lookup(&token), Lookup::Valid(_)));
        tokio::time::advance(TOKEN_TTL).await;
        assert!(matches!(tokens.lookup(&token), Lookup::Expired));
        assert!(matches!(tokens.lookup("nope"), Lookup::Unknown));
    }
}
