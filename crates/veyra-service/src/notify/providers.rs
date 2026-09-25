//! Notification providers: what each needs and how a message is sent.
//!
//! Each provider is a narrow, vendor-specific implementation behind
//! [`send`]; nothing outside this module knows a provider's wire format.
//! Every HTTP provider uses the notifier's one shared client (explicit
//! connect and total timeouts). Errors never include request URLs, because
//! Telegram bot tokens and webhook secrets live in them.

use std::collections::BTreeMap;
use std::time::Duration;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Notification, Rejection, Severity, reject};

/// A notification destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// SMTP email.
    Email,
    /// Telegram bot message.
    Telegram,
    /// Discord channel webhook.
    Discord,
    /// Slack incoming webhook.
    Slack,
    /// ntfy push notification (ntfy.sh or self-hosted).
    Ntfy,
    /// Pushover push notification.
    Pushover,
    /// JSON POST to any URL.
    Webhook,
}

/// One setting a provider takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// Wire name, camelCase.
    pub name: &'static str,
    /// Stored sealed and never returned.
    pub secret: bool,
    /// Must be set before the provider can be enabled.
    pub required: bool,
}

const fn field(name: &'static str, secret: bool, required: bool) -> FieldSpec {
    FieldSpec {
        name,
        secret,
        required,
    }
}

const EMAIL: &[FieldSpec] = &[
    field("host", false, true),
    field("port", false, false),
    field("security", false, false),
    field("username", false, false),
    field("from", false, true),
    field("to", false, true),
    field("password", true, false),
];
const TELEGRAM: &[FieldSpec] = &[field("chatId", false, true), field("botToken", true, true)];
const DISCORD: &[FieldSpec] = &[field("webhookUrl", true, true)];
const SLACK: &[FieldSpec] = &[field("webhookUrl", true, true)];
const NTFY: &[FieldSpec] = &[
    field("server", false, false),
    field("topic", true, true),
    field("accessToken", true, false),
];
const PUSHOVER: &[FieldSpec] = &[field("appToken", true, true), field("userKey", true, true)];
const WEBHOOK: &[FieldSpec] = &[field("url", true, true), field("bearerToken", true, false)];

/// Default ntfy server.
pub const NTFY_DEFAULT_SERVER: &str = "https://ntfy.sh";

/// Default SMTP submission port (STARTTLS).
const SMTP_DEFAULT_PORT: u16 = 587;

impl ProviderKind {
    /// Every provider, in display order.
    pub const ALL: [Self; 7] = [
        Self::Email,
        Self::Telegram,
        Self::Discord,
        Self::Slack,
        Self::Ntfy,
        Self::Pushover,
        Self::Webhook,
    ];

    /// Stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Ntfy => "ntfy",
            Self::Pushover => "pushover",
            Self::Webhook => "webhook",
        }
    }

    /// Parses a wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }

    /// The settings this provider takes.
    pub fn fields(self) -> &'static [FieldSpec] {
        match self {
            Self::Email => EMAIL,
            Self::Telegram => TELEGRAM,
            Self::Discord => DISCORD,
            Self::Slack => SLACK,
            Self::Ntfy => NTFY,
            Self::Pushover => PUSHOVER,
            Self::Webhook => WEBHOOK,
        }
    }

    /// One setting by wire name.
    pub fn field(self, name: &str) -> Option<FieldSpec> {
        self.fields().iter().copied().find(|spec| spec.name == name)
    }

    /// Checks merged fields and secrets for an enabled provider: required
    /// values present and every value well formed.
    pub fn validate(self, values: &BTreeMap<String, String>) -> Vec<Rejection> {
        let name = self.as_str();
        let mut rejected: Vec<Rejection> = self
            .fields()
            .iter()
            .filter(|spec| spec.required && !values.contains_key(spec.name))
            .map(|spec| reject(format!("providers.{name}.{}", spec.name), "required"))
            .collect();
        let at = |field: &str| format!("providers.{name}.{field}");
        let get = |field: &str| values.get(field).map(String::as_str);
        match self {
            Self::Email => {
                if let Some(port) = get("port")
                    && port.parse::<u16>().ok().filter(|port| *port > 0).is_none()
                {
                    rejected.push(reject(at("port"), "must be a port number, e.g. 587"));
                }
                if let Some(security) = get("security")
                    && SmtpSecurity::parse(security).is_none()
                {
                    rejected.push(reject(at("security"), "must be starttls or tls"));
                }
                if let Some(from) = get("from")
                    && from.parse::<Mailbox>().is_err()
                {
                    rejected.push(reject(at("from"), "must be an email address"));
                }
                if let Some(to) = get("to")
                    && recipients(to).is_err()
                {
                    rejected.push(reject(
                        at("to"),
                        "must be one or more email addresses, comma separated",
                    ));
                }
                if get("username").is_some() != get("password").is_some() {
                    rejected.push(reject(
                        at("password"),
                        "set both username and password, or neither",
                    ));
                }
            }
            Self::Telegram => {
                if let Some(chat) = get("chatId")
                    && !valid_chat_id(chat)
                {
                    rejected.push(reject(
                        at("chatId"),
                        "must be a numeric chat id or an @channel name",
                    ));
                }
            }
            Self::Discord => {
                if let Some(url) = get("webhookUrl")
                    && !https_on(url, &["discord.com", "discordapp.com"], "/api/webhooks/")
                {
                    rejected.push(reject(
                        at("webhookUrl"),
                        "must be a Discord webhook URL (https://discord.com/api/webhooks/…)",
                    ));
                }
            }
            Self::Slack => {
                if let Some(url) = get("webhookUrl")
                    && !https_on(url, &["hooks.slack.com"], "/services/")
                {
                    rejected.push(reject(
                        at("webhookUrl"),
                        "must be a Slack incoming webhook URL (https://hooks.slack.com/services/…)",
                    ));
                }
            }
            Self::Ntfy => {
                if let Some(server) = get("server")
                    && reqwest::Url::parse(server)
                        .ok()
                        .filter(|url| matches!(url.scheme(), "https" | "http") && url.has_host())
                        .is_none()
                {
                    rejected.push(reject(at("server"), "must be a URL, e.g. https://ntfy.sh"));
                }
                if let Some(topic) = get("topic")
                    && !topic
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    rejected.push(reject(at("topic"), "use letters, digits, - and _ only"));
                }
            }
            Self::Pushover => {}
            Self::Webhook => {
                if let Some(url) = get("url")
                    && reqwest::Url::parse(url)
                        .ok()
                        .filter(|url| url.scheme() == "https" && url.has_host())
                        .is_none()
                {
                    rejected.push(reject(at("url"), "must be an https:// URL"));
                }
            }
        }
        rejected
    }
}

fn valid_chat_id(raw: &str) -> bool {
    match raw.strip_prefix('@') {
        Some(channel) => {
            !channel.is_empty()
                && channel
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => raw.parse::<i64>().is_ok(),
    }
}

fn https_on(raw: &str, hosts: &[&str], path_prefix: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| hosts.contains(&host))
            && url.path().starts_with(path_prefix)
    })
}

fn recipients(raw: &str) -> Result<Vec<Mailbox>, lettre::address::AddressError> {
    raw.split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(str::parse::<Mailbox>)
        .collect::<Result<Vec<_>, _>>()
        .and_then(|list| {
            if list.is_empty() {
                Err(lettre::address::AddressError::MissingParts)
            } else {
                Ok(list)
            }
        })
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpSecurity {
    /// Plain connection upgraded with STARTTLS (usually port 587).
    StartTls,
    /// TLS from the first byte (usually port 465).
    Tls,
}

impl SmtpSecurity {
    fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::StartTls),
            "tls" | "ssl" => Some(Self::Tls),
            _ => None,
        }
    }
}

/// Why a delivery failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// Worth retrying: a timeout, connection failure, rate limit, or server
    /// error.
    #[error("{0}")]
    Transient(String),
    /// Retrying will not help: bad credentials, bad address, or a rejected
    /// request.
    #[error("{0}")]
    Permanent(String),
}

impl SendError {
    /// Whether a retry could succeed.
    pub fn transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

fn transport_error(error: reqwest::Error) -> SendError {
    SendError::Transient(error.without_url().to_string())
}

async fn check_status(response: reqwest::Response) -> Result<(), SendError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    let detail = format!(
        "HTTP {}: {}",
        status.as_u16(),
        crate::text::clip(body.trim(), 200)
    );
    if status.as_u16() == 429 || status.is_server_error() {
        Err(SendError::Transient(detail))
    } else {
        Err(SendError::Permanent(detail))
    }
}

/// Title and body as one chat message.
fn chat_text(notification: &Notification, bold: (&str, &str)) -> String {
    let (open, close) = bold;
    if notification.body.is_empty() {
        format!(
            "{} {open}{}{close}",
            notification.severity.marker(),
            notification.title
        )
    } else {
        format!(
            "{} {open}{}{close}\n{}",
            notification.severity.marker(),
            notification.title,
            notification.body
        )
    }
}

/// Sends one notification to one provider using its merged fields.
///
/// # Errors
/// Returns [`SendError`] when a required value is missing or the provider
/// does not accept the message.
pub async fn send(
    kind: ProviderKind,
    client: &reqwest::Client,
    values: &BTreeMap<String, String>,
    notification: &Notification,
) -> Result<(), SendError> {
    let get = |name: &'static str| {
        values
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| SendError::Permanent(format!("{name} is not set")))
    };
    match kind {
        ProviderKind::Telegram => {
            let url = format!(
                "https://api.telegram.org/bot{}/sendMessage",
                get("botToken")?
            );
            let response = client
                .post(url)
                .json(&json!({
                    "chat_id": get("chatId")?,
                    "text": chat_text(notification, ("", "")),
                    "disable_web_page_preview": true,
                }))
                .send()
                .await
                .map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Discord => {
            let response = client
                .post(get("webhookUrl")?)
                .json(&json!({
                    "content": chat_text(notification, ("**", "**")),
                    "allowed_mentions": { "parse": [] },
                }))
                .send()
                .await
                .map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Slack => {
            let response = client
                .post(get("webhookUrl")?)
                .json(&json!({ "text": chat_text(notification, ("*", "*")) }))
                .send()
                .await
                .map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Ntfy => {
            let server = values
                .get("server")
                .map_or(NTFY_DEFAULT_SERVER, String::as_str)
                .trim_end_matches('/');
            let priority = match notification.severity {
                Severity::Info => 3,
                Severity::Warning => 4,
                Severity::Critical => 5,
            };
            let mut request = client.post(server).json(&json!({
                "topic": get("topic")?,
                "title": notification.title,
                "message": if notification.body.is_empty() { &notification.title } else { &notification.body },
                "priority": priority,
                "tags": [notification.event_name()],
            }));
            if let Some(token) = values.get("accessToken") {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Pushover => {
            let priority = match notification.severity {
                Severity::Info => 0,
                Severity::Warning => 0,
                Severity::Critical => 1,
            };
            let response = client
                .post("https://api.pushover.net/1/messages.json")
                .json(&json!({
                    "token": get("appToken")?,
                    "user": get("userKey")?,
                    "title": notification.title,
                    "message": if notification.body.is_empty() { &notification.title } else { &notification.body },
                    "priority": priority,
                }))
                .send()
                .await
                .map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Webhook => {
            let mut request = client.post(get("url")?).json(&json!({
                "service": "veyra",
                "event": notification.event_name(),
                "severity": notification.severity,
                "title": notification.title,
                "body": notification.body,
                "at": notification.at.unix_timestamp(),
            }));
            if let Some(token) = values.get("bearerToken") {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(transport_error)?;
            check_status(response).await
        }
        ProviderKind::Email => send_email(values, notification).await,
    }
}

async fn send_email(
    values: &BTreeMap<String, String>,
    notification: &Notification,
) -> Result<(), SendError> {
    let permanent = |reason: &str| SendError::Permanent(reason.to_owned());
    let host = values
        .get("host")
        .ok_or_else(|| permanent("host is not set"))?;
    let from: Mailbox = values
        .get("from")
        .ok_or_else(|| permanent("from is not set"))?
        .parse()
        .map_err(|_| permanent("from is not an email address"))?;
    let to = recipients(values.get("to").map_or("", String::as_str))
        .map_err(|_| permanent("to is not a list of email addresses"))?;
    let port = values
        .get("port")
        .map(|port| port.parse::<u16>())
        .transpose()
        .map_err(|_| permanent("port is not a number"))?
        .unwrap_or(SMTP_DEFAULT_PORT);
    let security = values
        .get("security")
        .map(|raw| SmtpSecurity::parse(raw).ok_or_else(|| permanent("security is invalid")))
        .transpose()?
        .unwrap_or(if port == 465 {
            SmtpSecurity::Tls
        } else {
            SmtpSecurity::StartTls
        });

    let mut message = Message::builder()
        .from(from)
        .subject(format!("[Veyra] {}", notification.title));
    for recipient in to {
        message = message.to(recipient);
    }
    let body = if notification.body.is_empty() {
        notification.title.clone()
    } else {
        notification.body.clone()
    };
    let message = message
        .body(body)
        .map_err(|error| SendError::Permanent(error.to_string()))?;

    let builder = match security {
        SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
    }
    .map_err(|error| SendError::Permanent(error.to_string()))?;
    let mut builder = builder.port(port).timeout(Some(Duration::from_secs(15)));
    if let (Some(username), Some(password)) = (values.get("username"), values.get("password")) {
        builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
    }
    builder
        .build()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|error| {
            if error.is_transient() || error.is_timeout() {
                SendError::Transient(error.to_string())
            } else {
                SendError::Permanent(error.to_string())
            }
        })
}
