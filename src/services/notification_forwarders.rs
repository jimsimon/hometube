//! Forwarding of in-app parent notifications to a self-hosted push
//! service (Apprise, ntfy.sh, or Gotify).
//!
//! Storage: a single JSON blob in `app_config` under
//! [`KEY_NOTIFICATION_SERVICE`]. The blob deserialises into
//! [`ForwardingSettings`].
//!
//! Delivery: [`forward_if_enabled`] is called from
//! [`crate::services::notifications`] after a successful in-app insert.
//! It loads the config, checks the per-type allowlist, and
//! `tokio::spawn`s an HTTP request via a shared `reqwest::Client`.
//! Failures are logged via `tracing` and never propagated — external
//! delivery must not block or fail the in-app path.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{debug, error, warn};

use crate::error::{AppError, AppResult};
use crate::services::setup;

/// `app_config` key that stores the serialised [`ForwardingSettings`].
pub const KEY_NOTIFICATION_SERVICE: &str = "notification_service";

/// Sentinel returned in place of secrets on `GET` responses, and used by
/// the UI when the operator wants to keep an existing secret while
/// editing other fields.
pub const SECRET_PLACEHOLDER: &str = "********";

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Provider-specific connection details.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ForwarderConfig {
    Ntfy {
        base_url: String,
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<u8>,
    },
    Gotify {
        base_url: String,
        app_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<u8>,
    },
    Apprise {
        base_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        urls: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        basic_auth_user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        basic_auth_password: Option<String>,
    },
}

/// Full forwarder configuration persisted in `app_config`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ForwardingSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: Option<ForwarderConfig>,
    /// Allowlist of `notification_type` strings to forward. Empty list
    /// means "forward nothing" (a fail-safe default).
    #[serde(default)]
    pub enabled_types: Vec<String>,
}

/// Shared HTTP client for all outbound forwarder requests. Lazily
/// initialised so test code that never touches the forwarder doesn't
/// pay the connection-pool setup cost.
pub fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent("HomeTube/1.0 notification-forwarder")
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

/// Load the persisted [`ForwardingSettings`], returning the default
/// (disabled, no provider) when the key is absent or the stored JSON
/// fails to deserialise.
pub async fn load(pool: &SqlitePool) -> AppResult<ForwardingSettings> {
    let raw = setup::get_config_value(pool, KEY_NOTIFICATION_SERVICE).await?;
    match raw {
        Some(s) => match serde_json::from_str::<ForwardingSettings>(&s) {
            Ok(cfg) => Ok(cfg),
            Err(err) => {
                warn!(error = %err, "notification forwarder config is malformed; ignoring");
                Ok(ForwardingSettings::default())
            }
        },
        None => Ok(ForwardingSettings::default()),
    }
}

/// Persist the [`ForwardingSettings`] as JSON in `app_config`.
pub async fn save(pool: &SqlitePool, settings: &ForwardingSettings) -> AppResult<()> {
    let json = serde_json::to_string(settings)
        .map_err(|e| AppError::Other(anyhow::anyhow!("serialise forwarder settings: {e}")))?;
    setup::set_config_value(pool, KEY_NOTIFICATION_SERVICE, &json).await
}

/// Return a copy of `settings` with every secret replaced by
/// [`SECRET_PLACEHOLDER`], suitable for sending to the frontend.
pub fn redact(settings: &ForwardingSettings) -> ForwardingSettings {
    let provider = settings.provider.as_ref().map(|p| match p.clone() {
        ForwarderConfig::Ntfy {
            base_url,
            topic,
            token,
            priority,
        } => ForwarderConfig::Ntfy {
            base_url,
            topic,
            token: token.map(|_| SECRET_PLACEHOLDER.to_string()),
            priority,
        },
        ForwarderConfig::Gotify {
            base_url,
            app_token: _,
            priority,
        } => ForwarderConfig::Gotify {
            base_url,
            // Always emit the placeholder so the UI can round-trip
            // GET → PUT without the merge step seeing an empty token
            // and tripping validation. `merge_secrets` swaps it back
            // before persistence.
            app_token: SECRET_PLACEHOLDER.to_string(),
            priority,
        },
        ForwarderConfig::Apprise {
            base_url,
            config_key,
            urls,
            basic_auth_user,
            basic_auth_password,
        } => ForwarderConfig::Apprise {
            base_url,
            config_key,
            urls,
            basic_auth_user,
            basic_auth_password: basic_auth_password.map(|_| SECRET_PLACEHOLDER.to_string()),
        },
    });
    ForwardingSettings {
        enabled: settings.enabled,
        provider,
        enabled_types: settings.enabled_types.clone(),
    }
}

/// Merge an `incoming` settings payload from the UI with the existing
/// stored settings: any secret field whose value equals
/// [`SECRET_PLACEHOLDER`] is replaced with the previously stored value.
/// This lets the UI round-trip GET → PUT without re-typing secrets.
pub fn merge_secrets(
    existing: &ForwardingSettings,
    mut incoming: ForwardingSettings,
) -> ForwardingSettings {
    if let (Some(new_p), Some(old_p)) = (incoming.provider.as_mut(), existing.provider.as_ref()) {
        match (new_p, old_p) {
            (
                ForwarderConfig::Ntfy {
                    token: new_token, ..
                },
                ForwarderConfig::Ntfy {
                    token: old_token, ..
                },
            ) => {
                if matches!(new_token.as_deref(), Some(SECRET_PLACEHOLDER)) {
                    *new_token = old_token.clone();
                }
            }
            (
                ForwarderConfig::Gotify {
                    app_token: new_t, ..
                },
                ForwarderConfig::Gotify {
                    app_token: old_t, ..
                },
            ) if new_t == SECRET_PLACEHOLDER => {
                *new_t = old_t.clone();
            }
            (
                ForwarderConfig::Apprise {
                    basic_auth_password: new_p,
                    ..
                },
                ForwarderConfig::Apprise {
                    basic_auth_password: old_p,
                    ..
                },
            ) => {
                if matches!(new_p.as_deref(), Some(SECRET_PLACEHOLDER)) {
                    *new_p = old_p.clone();
                }
            }
            _ => {}
        }
    }
    incoming
}

/// Validate a settings payload before persisting. Returns
/// [`AppError::BadRequest`] with a human-readable message on failure.
pub fn validate(settings: &ForwardingSettings) -> AppResult<()> {
    if settings.enabled && settings.provider.is_none() {
        return Err(AppError::BadRequest(
            "provider must be set when enabled is true".into(),
        ));
    }
    if let Some(provider) = &settings.provider {
        match provider {
            ForwarderConfig::Ntfy {
                base_url,
                topic,
                priority,
                ..
            } => {
                check_url(base_url)?;
                if topic.trim().is_empty() {
                    return Err(AppError::BadRequest("ntfy topic must not be empty".into()));
                }
                if let Some(p) = priority {
                    if !(1..=5).contains(p) {
                        return Err(AppError::BadRequest(
                            "ntfy priority must be between 1 and 5".into(),
                        ));
                    }
                }
            }
            ForwarderConfig::Gotify {
                base_url,
                app_token,
                priority,
            } => {
                check_url(base_url)?;
                if app_token.trim().is_empty() {
                    return Err(AppError::BadRequest(
                        "gotify app_token must not be empty".into(),
                    ));
                }
                if let Some(p) = priority {
                    if *p > 10 {
                        return Err(AppError::BadRequest(
                            "gotify priority must be between 0 and 10".into(),
                        ));
                    }
                }
            }
            ForwarderConfig::Apprise {
                base_url,
                config_key,
                urls,
                ..
            } => {
                check_url(base_url)?;
                let has_key = config_key
                    .as_deref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                let has_urls = urls
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if !has_key && !has_urls {
                    return Err(AppError::BadRequest(
                        "apprise requires either config_key or urls".into(),
                    ));
                }
                if let Some(k) = config_key.as_deref() {
                    if !k.is_empty() && !is_safe_apprise_key(k) {
                        return Err(AppError::BadRequest(
                            "apprise config_key may only contain letters, digits, '-' and '_'"
                                .into(),
                        ));
                    }
                }
            }
        }
    }
    for t in &settings.enabled_types {
        if !KNOWN_TYPES.contains(&t.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unknown notification type: {t}"
            )));
        }
    }
    Ok(())
}

/// Restrict Apprise stateful config keys to safe URL path characters,
/// since the key is interpolated into `/notify/{key}`. Apprise itself
/// allows letters, digits, `-`, and `_` in keys, so this is the right
/// alphabet and also forecloses path traversal.
fn is_safe_apprise_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_url(url: &str) -> AppResult<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| AppError::BadRequest(format!("invalid url '{url}': {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest(format!(
            "url '{url}' must use http or https"
        )));
    }
    Ok(())
}

/// Notification-type strings accepted in `enabled_types`. Mirrors the
/// `parent_notifications.notification_type` CHECK constraint.
pub const KNOWN_TYPES: &[&str] = &[
    "time_limit_approaching",
    "time_limit_reached",
    "ytdlp_failure",
    "sync_error",
    "token_expired",
    "new_search_term",
    "system_update",
];

/// Top-level entry point used by the in-app notification dispatcher.
///
/// Loads the persisted config, checks whether the given
/// `notification_type` is in the allowlist, and spawns a background
/// task that performs the HTTP push. Never blocks the caller for more
/// than a SQL read.
pub async fn forward_if_enabled(
    pool: &SqlitePool,
    notification_type: &str,
    title: &str,
    message: &str,
) {
    let settings = match load(pool).await {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "failed to load notification forwarder settings");
            return;
        }
    };
    if !settings.enabled {
        return;
    }
    if !settings
        .enabled_types
        .iter()
        .any(|t| t == notification_type)
    {
        return;
    }
    let Some(provider) = settings.provider else {
        return;
    };
    let kind = notification_type.to_string();
    let title = title.to_string();
    let message = message.to_string();
    tokio::spawn(async move {
        if let Err(err) = send(shared_client(), &provider, &title, &message, &kind).await {
            error!(target: "notification_forwarder", error = %err, kind = %kind, "delivery failed");
        } else {
            debug!(target: "notification_forwarder", kind = %kind, "delivery ok");
        }
    });
}

/// Perform the actual HTTP push for a single notification. Public so
/// the `/api/notifications/config/test` endpoint can call it
/// synchronously and return a real error string to the UI.
/// One notification payload as seen by the provider-specific senders.
struct Message<'a> {
    title: &'a str,
    body: &'a str,
    kind: &'a str,
}

pub async fn send(
    client: &Client,
    provider: &ForwarderConfig,
    title: &str,
    message: &str,
    kind: &str,
) -> Result<(), String> {
    let msg = Message {
        title,
        body: message,
        kind,
    };
    match provider {
        ForwarderConfig::Ntfy { .. } => send_ntfy(client, provider, &msg).await,
        ForwarderConfig::Gotify { .. } => send_gotify(client, provider, &msg).await,
        ForwarderConfig::Apprise { .. } => send_apprise(client, provider, &msg).await,
    }
}

fn ntfy_priority_for(kind: &str, default: Option<u8>) -> u8 {
    if let Some(p) = default {
        return p;
    }
    match kind {
        "ytdlp_failure" | "time_limit_reached" | "sync_error" | "token_expired" => 4,
        "time_limit_approaching" => 3,
        "new_search_term" => 2,
        _ => 3,
    }
}

fn gotify_priority_for(kind: &str, default: Option<u8>) -> u8 {
    if let Some(p) = default {
        return p;
    }
    match kind {
        "ytdlp_failure" | "time_limit_reached" | "sync_error" | "token_expired" => 8,
        "time_limit_approaching" => 5,
        "new_search_term" => 3,
        _ => 5,
    }
}

async fn send_ntfy(
    client: &Client,
    provider: &ForwarderConfig,
    msg: &Message<'_>,
) -> Result<(), String> {
    let ForwarderConfig::Ntfy {
        base_url,
        topic,
        token,
        priority,
    } = provider
    else {
        unreachable!("send_ntfy called with non-Ntfy provider")
    };
    let url = format!("{}/{}", base_url.trim_end_matches('/'), topic);
    // HTTP/1.1 header values are restricted to visible ASCII (+ space
    // and horizontal tab). ntfy itself accepts RFC 2047 encoded-words
    // for the `Title` header, so wrap non-ASCII titles in
    // `=?UTF-8?B?<base64>?=`.
    let encoded_title = encode_header_value(msg.title);
    let mut req = client
        .post(&url)
        .header("Title", encoded_title)
        .header(
            "Priority",
            ntfy_priority_for(msg.kind, *priority).to_string(),
        )
        .header("Tags", msg.kind)
        .body(msg.body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    check_response(resp).await
}

async fn send_gotify(
    client: &Client,
    provider: &ForwarderConfig,
    msg: &Message<'_>,
) -> Result<(), String> {
    let ForwarderConfig::Gotify {
        base_url,
        app_token,
        priority,
    } = provider
    else {
        unreachable!("send_gotify called with non-Gotify provider")
    };
    let url = format!("{}/message", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "title": msg.title,
        "message": msg.body,
        "priority": gotify_priority_for(msg.kind, *priority),
    });
    // Send the app token in the `X-Gotify-Key` header rather than as a
    // query parameter, so it doesn't end up in reverse-proxy access
    // logs or browser URL history.
    let resp = client
        .post(&url)
        .header("X-Gotify-Key", app_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    check_response(resp).await
}

async fn send_apprise(
    client: &Client,
    provider: &ForwarderConfig,
    msg: &Message<'_>,
) -> Result<(), String> {
    let ForwarderConfig::Apprise {
        base_url,
        config_key,
        urls,
        basic_auth_user,
        basic_auth_password,
    } = provider
    else {
        unreachable!("send_apprise called with non-Apprise provider")
    };
    let path = match config_key.as_deref() {
        Some(k) if !k.is_empty() => format!("/notify/{}", k),
        _ => "/notify".to_string(),
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let apprise_type = match msg.kind {
        "ytdlp_failure" | "sync_error" | "token_expired" | "time_limit_reached" => "failure",
        "time_limit_approaching" => "warning",
        "system_update" => "info",
        _ => "info",
    };
    let mut body = serde_json::json!({
        "title": msg.title,
        "body": msg.body,
        "type": apprise_type,
        "tag": msg.kind,
    });
    if let Some(u) = urls.as_deref() {
        if !u.trim().is_empty() {
            body["urls"] = serde_json::Value::String(u.to_string());
        }
    }
    let mut req = client.post(&url).json(&body);
    if let (Some(u), Some(p)) = (basic_auth_user.as_deref(), basic_auth_password.as_deref()) {
        req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    check_response(resp).await
}

/// Encode `value` for safe inclusion in an HTTP header. ASCII inputs
/// pass through unchanged; non-ASCII inputs are wrapped in an RFC 2047
/// base64 encoded-word (`=?UTF-8?B?<base64>?=`).
fn encode_header_value(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    use base64::{engine::general_purpose::STANDARD, Engine};
    format!("=?UTF-8?B?{}?=", STANDARD.encode(value.as_bytes()))
}

async fn check_response(resp: reqwest::Response) -> Result<(), String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    let truncated: String = body.chars().take(200).collect();
    Err(format!("HTTP {status}: {truncated}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntfy_cfg() -> ForwardingSettings {
        ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Ntfy {
                base_url: "https://ntfy.example".into(),
                topic: "kids".into(),
                token: Some("super-secret".into()),
                priority: Some(3),
            }),
            enabled_types: vec!["ytdlp_failure".into()],
        }
    }

    #[test]
    fn redact_replaces_ntfy_token() {
        let r = redact(&ntfy_cfg());
        match r.provider.unwrap() {
            ForwarderConfig::Ntfy { token, .. } => {
                assert_eq!(token.as_deref(), Some(SECRET_PLACEHOLDER));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn merge_keeps_existing_secret_on_placeholder() {
        let existing = ntfy_cfg();
        let mut incoming = redact(&existing);
        // simulate user changing topic but leaving token as placeholder
        if let Some(ForwarderConfig::Ntfy { topic, .. }) = &mut incoming.provider {
            *topic = "new-topic".into();
        }
        let merged = merge_secrets(&existing, incoming);
        match merged.provider.unwrap() {
            ForwarderConfig::Ntfy { token, topic, .. } => {
                assert_eq!(token.as_deref(), Some("super-secret"));
                assert_eq!(topic, "new-topic");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn validate_rejects_enabled_without_provider() {
        let s = ForwardingSettings {
            enabled: true,
            provider: None,
            enabled_types: vec![],
        };
        assert!(validate(&s).is_err());
    }

    #[test]
    fn validate_rejects_bad_url() {
        let s = ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Ntfy {
                base_url: "not a url".into(),
                topic: "x".into(),
                token: None,
                priority: None,
            }),
            enabled_types: vec![],
        };
        assert!(validate(&s).is_err());
    }

    #[test]
    fn validate_rejects_unknown_type() {
        let s = ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Ntfy {
                base_url: "https://ntfy.example".into(),
                topic: "x".into(),
                token: None,
                priority: None,
            }),
            enabled_types: vec!["bogus".into()],
        };
        assert!(validate(&s).is_err());
    }

    #[test]
    fn encode_header_value_passes_ascii_through() {
        assert_eq!(encode_header_value("Hello"), "Hello");
    }

    #[test]
    fn encode_header_value_base64_wraps_unicode() {
        let out = encode_header_value("Héllo");
        assert!(out.starts_with("=?UTF-8?B?"));
        assert!(out.ends_with("?="));
    }

    #[test]
    fn redact_gotify_always_emits_placeholder() {
        let s = ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Gotify {
                base_url: "https://gotify.example".into(),
                app_token: "real-token".into(),
                priority: None,
            }),
            enabled_types: vec![],
        };
        let r = redact(&s);
        match r.provider.unwrap() {
            ForwarderConfig::Gotify { app_token, .. } => {
                assert_eq!(app_token, SECRET_PLACEHOLDER);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn validate_rejects_unsafe_apprise_key() {
        for bad in ["../etc", "a/b", "a b", "a.b", ""] {
            let s = ForwardingSettings {
                enabled: true,
                provider: Some(ForwarderConfig::Apprise {
                    base_url: "https://apprise.example".into(),
                    config_key: Some(bad.into()),
                    urls: None,
                    basic_auth_user: None,
                    basic_auth_password: None,
                }),
                enabled_types: vec![],
            };
            // Empty key is rejected because neither key nor urls is set;
            // the others are rejected by the safety check.
            assert!(validate(&s).is_err(), "expected error for key {bad:?}");
        }
    }

    #[test]
    fn validate_accepts_safe_apprise_key() {
        let s = ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Apprise {
                base_url: "https://apprise.example".into(),
                config_key: Some("family_kids-1".into()),
                urls: None,
                basic_auth_user: None,
                basic_auth_password: None,
            }),
            enabled_types: vec![],
        };
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn validate_accepts_valid_gotify() {
        let s = ForwardingSettings {
            enabled: true,
            provider: Some(ForwarderConfig::Gotify {
                base_url: "https://gotify.example".into(),
                app_token: "abc".into(),
                priority: Some(5),
            }),
            enabled_types: vec!["ytdlp_failure".into()],
        };
        assert!(validate(&s).is_ok());
    }
}
