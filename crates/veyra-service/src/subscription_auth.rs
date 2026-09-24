//! PKCE authorization and token exchange for Codex and Claude Code subscriptions.
//!
//! This module only handles the interactive credential boundary. Tokens are
//! opaque to the HTTP layer and must be encrypted by [`crate::credential`]
//! before persistence. Model transport and refresh are owned by the provider
//! runtime, so a successful OAuth exchange does not imply live model access.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};

const CODEX_ISSUER: &str = "https://auth.openai.com";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CLAUDE_AUTHORIZE_URL: &str = "https://platform.claude.com/oauth/authorize";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const CLAUDE_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Subscription provider supported by the OAuth boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionProvider {
    /// OpenAI Codex/ChatGPT subscription.
    Codex,
    /// Anthropic Claude Code subscription.
    ClaudeCode,
}

impl SubscriptionProvider {
    /// Parses the public provider name.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude_code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }
    /// Returns the stable public provider name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
        }
    }
}

/// Short-lived state required to complete one PKCE exchange.
#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    /// Provider being authorized.
    pub provider: SubscriptionProvider,
    /// One-time CSRF state.
    pub state: String,
    /// PKCE verifier retained only until completion.
    pub verifier: String,
    /// Browser authorization URL.
    pub authorize_url: String,
    /// Creation time used for expiry.
    pub created_at: SystemTime,
}

/// Opaque exchanged subscription credential. Do not serialize this into HTTP responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionCredential {
    /// Provider which issued the tokens.
    pub provider: SubscriptionProvider,
    /// Provider access token.
    pub access_token: String,
    /// Codex identity token used to recover the account routing ID.
    #[serde(default)]
    pub id_token: String,
    /// Refresh token, when supplied.
    #[serde(default)]
    pub refresh_token: String,
    /// ChatGPT account routing ID, when present.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Claude account UUID, when present.
    #[serde(default)]
    pub account_uuid: Option<String>,
    /// Claude organization UUID, when present.
    #[serde(default)]
    pub organization_uuid: Option<String>,
    /// OAuth scopes returned by Claude.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Optional provider account label.
    #[serde(default)]
    pub account_label: Option<String>,
    /// Unix expiry of the access token.
    #[serde(default)]
    pub expires_at_unix: Option<i64>,
}

impl SubscriptionCredential {
    /// Rejects credentials that cannot be used by the selected provider.
    ///
    /// Expired credentials remain valid when a refresh token is present; the
    /// model runtime refreshes those before its first request. An expired
    /// access token without a refresh token is treated as disconnected.
    pub fn validate_for(&self, provider: SubscriptionProvider) -> Result<()> {
        if self.provider != provider {
            bail!("subscription credential provider mismatch")
        }
        if self.access_token.trim().is_empty() {
            bail!("subscription credential omitted access token")
        }
        if self
            .expires_at_unix
            .is_some_and(|expiry| expiry <= now_unix() && self.refresh_token.trim().is_empty())
        {
            bail!("subscription credential is expired")
        }
        Ok(())
    }
}

/// In-memory OAuth attempts and exchanged credentials awaiting runtime adoption.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionAuthState(Arc<Mutex<SubscriptionAuthInner>>);

#[derive(Debug, Default)]
struct SubscriptionAuthInner {
    pending: HashMap<SubscriptionProvider, PendingAuthorization>,
    credentials: HashMap<SubscriptionProvider, SubscriptionCredential>,
}

impl SubscriptionAuthState {
    /// Creates an empty subscription state.
    pub fn new() -> Self {
        Self::default()
    }
    /// Stores a one-time pending flow.
    pub fn put_pending(&self, pending: PendingAuthorization) {
        if let Ok(mut state) = self.0.lock() {
            state.pending.insert(pending.provider, pending);
        }
    }
    /// Takes and validates a pending flow.
    pub fn take_pending(
        &self,
        provider: SubscriptionProvider,
        state_value: &str,
    ) -> Result<PendingAuthorization> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("subscription state unavailable"))?;
        let pending = state
            .pending
            .get(&provider)
            .context("subscription authorization was not started")?;
        validate_pending(pending, provider, state_value)?;
        state
            .pending
            .remove(&provider)
            .context("subscription authorization was not started")
    }
    /// Stores an exchanged credential in memory until durable persistence succeeds.
    pub fn set_credential(&self, credential: SubscriptionCredential) {
        if let Ok(mut state) = self.0.lock() {
            state.credentials.insert(credential.provider, credential);
        }
    }
    /// Removes one provider credential.
    pub fn remove_credential(&self, provider: SubscriptionProvider) -> bool {
        self.0
            .lock()
            .map(|mut state| state.credentials.remove(&provider).is_some())
            .unwrap_or(false)
    }
    /// Returns a credential for runtime adoption without exposing it over HTTP.
    pub fn credential(&self, provider: SubscriptionProvider) -> Option<SubscriptionCredential> {
        self.0.lock().ok()?.credentials.get(&provider).cloned()
    }
    /// Returns whether a provider has a credential.
    pub fn connected(&self, provider: SubscriptionProvider) -> bool {
        self.0
            .lock()
            .map(|state| {
                state
                    .credentials
                    .get(&provider)
                    .is_some_and(|credential| credential.validate_for(provider).is_ok())
            })
            .unwrap_or(false)
    }
}

/// Builds a provider authorization URL and fresh PKCE state.
pub fn prepare(provider: SubscriptionProvider) -> Result<PendingAuthorization> {
    let verifier = random_urlsafe(32)?;
    let challenge =
        URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
    let state = random_urlsafe(24)?;
    let authorize_endpoint = match provider {
        SubscriptionProvider::Codex => format!("{CODEX_ISSUER}/oauth/authorize"),
        SubscriptionProvider::ClaudeCode => CLAUDE_AUTHORIZE_URL.to_owned(),
    };
    let mut url = reqwest::Url::parse(&authorize_endpoint)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair(
            "client_id",
            match provider {
                SubscriptionProvider::Codex => CODEX_CLIENT_ID,
                SubscriptionProvider::ClaudeCode => CLAUDE_CLIENT_ID,
            },
        )
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    match provider {
        SubscriptionProvider::Codex => {
            url.query_pairs_mut()
                .append_pair(
                    "scope",
                    "openid profile email offline_access api.connectors.read api.connectors.invoke",
                )
                .append_pair("codex_cli_simplified_flow", "true")
                .append_pair("originator", CODEX_ORIGINATOR)
                .append_pair("redirect_uri", CODEX_REDIRECT_URI);
        }
        SubscriptionProvider::ClaudeCode => {
            url.query_pairs_mut()
                .append_pair("code", "true")
                .append_pair("scope", CLAUDE_SCOPE)
                .append_pair("redirect_uri", CLAUDE_REDIRECT_URI);
        }
    }
    Ok(PendingAuthorization {
        provider,
        state,
        verifier,
        authorize_url: url.to_string(),
        created_at: SystemTime::now(),
    })
}

/// Rejects stale or mismatched callback state before an exchange.
pub fn validate_pending(
    pending: &PendingAuthorization,
    provider: SubscriptionProvider,
    state: &str,
) -> Result<()> {
    if pending.provider != provider || pending.state != state {
        bail!("subscription authorization state is invalid")
    }
    if pending.created_at.elapsed().unwrap_or(Duration::MAX) > Duration::from_secs(600) {
        bail!("subscription authorization expired")
    }
    Ok(())
}

/// Extracts authorization code and state from a pasted callback URL or blob.
pub fn callback_parts(provider: SubscriptionProvider, value: &str) -> Result<(String, String)> {
    let value = value.trim();
    if value.is_empty() {
        bail!("callback must include code and state")
    }
    if let Ok(url) = reqwest::Url::parse(value) {
        validate_callback_url(provider, &url)?;
        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned());
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        if let (Some(code), Some(state)) = (code, state)
            && !code.is_empty()
            && !state.is_empty()
        {
            return Ok((code, state));
        }
        bail!("callback must include code and state")
    }
    if value.contains("://") {
        bail!("callback URL is invalid")
    }
    let (code, state) = value
        .split_once('#')
        .context("callback must include code and state")?;
    let code = code.trim();
    let state = state.trim();
    if code.is_empty() || state.is_empty() || state.contains('#') {
        bail!("callback must include code and state")
    }
    Ok((code.to_owned(), state.to_owned()))
}

fn validate_callback_url(provider: SubscriptionProvider, url: &reqwest::Url) -> Result<()> {
    let (scheme, host, port, path) = match provider {
        SubscriptionProvider::Codex => ("http", "localhost", 1455, "/auth/callback"),
        SubscriptionProvider::ClaudeCode => {
            ("https", "platform.claude.com", 443, "/oauth/code/callback")
        }
    };
    if url.scheme() != scheme
        || url.host_str() != Some(host)
        || url.port_or_known_default() != Some(port)
        || url.path() != path
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("callback URL does not match the selected provider")
    }
    Ok(())
}

/// Exchanges an authorization code for an opaque subscription credential.
pub async fn exchange(
    client: &reqwest::Client,
    pending: &PendingAuthorization,
    code: &str,
) -> Result<SubscriptionCredential> {
    let response = match pending.provider {
        SubscriptionProvider::Codex => client.post(format!("{CODEX_ISSUER}/oauth/token")).header("originator", CODEX_ORIGINATOR).form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", CODEX_REDIRECT_URI), ("client_id", CODEX_CLIENT_ID), ("code_verifier", pending.verifier.as_str())]).send().await.context("Codex subscription exchange failed")?,
        SubscriptionProvider::ClaudeCode => client.post(CLAUDE_TOKEN_URL).json(&serde_json::json!({"grant_type":"authorization_code","client_id":CLAUDE_CLIENT_ID,"code":code,"redirect_uri":CLAUDE_REDIRECT_URI,"code_verifier":pending.verifier})).send().await.context("Claude subscription exchange failed")?,
    };
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("subscription exchange response malformed")?;
    if !status.is_success() {
        bail!("subscription exchange rejected ({status})")
    }
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("subscription exchange omitted access token")?
        .to_owned();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let expires_at_unix = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .map(|seconds| now_unix().saturating_add(seconds));
    let id_token = body
        .get("id_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let claims = jwt_claims(&id_token);
    let account_id = claims
        .as_ref()
        .and_then(|claims| claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let codex_email = claims
        .as_ref()
        .and_then(|claims| claims.get("email"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let claude_account = body.get("account");
    let claude_organization = body.get("organization");
    let account_uuid = claude_account
        .and_then(|account| account.get("uuid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let organization_uuid = claude_organization
        .and_then(|organization| organization.get("uuid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let claude_email = claude_account
        .and_then(|account| account.get("email_address"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let scopes = body
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let credential = SubscriptionCredential {
        provider: pending.provider,
        access_token,
        id_token,
        refresh_token,
        account_id,
        account_uuid,
        organization_uuid,
        scopes,
        account_label: codex_email.or(claude_email),
        expires_at_unix,
    };
    credential.validate_for(pending.provider)?;
    Ok(credential)
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let encoded = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut value = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| anyhow::anyhow!("secure randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_secs().try_into().unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_flow_has_pkce_and_provider_endpoint() {
        let codex = prepare(SubscriptionProvider::Codex).expect("codex flow");
        assert!(codex.authorize_url.starts_with(CODEX_ISSUER));
        assert!(codex.authorize_url.contains("code_challenge="));
        validate_pending(&codex, SubscriptionProvider::Codex, &codex.state).expect("state");

        let claude = prepare(SubscriptionProvider::ClaudeCode).expect("claude flow");
        assert!(claude.authorize_url.starts_with(CLAUDE_AUTHORIZE_URL));
        assert!(claude.authorize_url.contains("user%3Ainference"));
    }

    #[test]
    fn callback_state_is_bound_to_provider_and_value() {
        let pending = prepare(SubscriptionProvider::Codex).expect("flow");
        assert!(
            validate_pending(&pending, SubscriptionProvider::ClaudeCode, &pending.state).is_err()
        );
        assert!(validate_pending(&pending, SubscriptionProvider::Codex, "wrong-state").is_err());
    }

    #[test]
    fn callback_url_must_match_provider_redirect() {
        let valid = callback_parts(
            SubscriptionProvider::Codex,
            "http://localhost:1455/auth/callback?code=abc&state=xyz",
        )
        .expect("valid callback");
        assert_eq!(valid, ("abc".to_owned(), "xyz".to_owned()));
        assert!(
            callback_parts(
                SubscriptionProvider::Codex,
                "https://example.invalid/auth/callback?code=abc&state=xyz",
            )
            .is_err()
        );
        assert!(
            callback_parts(
                SubscriptionProvider::ClaudeCode,
                "http://localhost:1455/auth/callback?code=abc&state=xyz",
            )
            .is_err()
        );
        assert_eq!(
            callback_parts(SubscriptionProvider::Codex, "abc#xyz").expect("pasted callback"),
            ("abc".to_owned(), "xyz".to_owned())
        );
    }

    #[test]
    fn invalid_callback_does_not_consume_pending_flow() {
        let auth = SubscriptionAuthState::new();
        let pending = prepare(SubscriptionProvider::Codex).expect("flow");
        let state = pending.state.clone();
        auth.put_pending(pending);
        assert!(
            auth.take_pending(SubscriptionProvider::Codex, "wrong")
                .is_err()
        );
        assert!(
            auth.take_pending(SubscriptionProvider::Codex, &state)
                .is_ok()
        );
    }

    #[test]
    fn expired_credential_without_refresh_is_disconnected() {
        let auth = SubscriptionAuthState::new();
        auth.set_credential(SubscriptionCredential {
            provider: SubscriptionProvider::Codex,
            access_token: "access".to_owned(),
            id_token: String::new(),
            refresh_token: String::new(),
            account_id: None,
            account_uuid: None,
            organization_uuid: None,
            scopes: Vec::new(),
            account_label: None,
            expires_at_unix: Some(now_unix().saturating_sub(1)),
        });
        assert!(!auth.connected(SubscriptionProvider::Codex));
    }
}
