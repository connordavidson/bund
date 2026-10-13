use crate::{failure, protocol, sse::Decoder};
use bund_core::{Error, ProviderErrorKind, ProviderEvent, Result};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, RETRY_AFTER},
    Client, Url,
};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 8192;
const MAX_RETRIES: u32 = 2;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) enum WireProtocol {
    OpenAi,
    Anthropic,
}

pub(crate) struct Transport {
    client: Client,
    key: String,
}

impl Transport {
    pub fn new(key: String) -> Self {
        Self {
            key,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("TLS client configuration is valid"),
        }
    }

    pub async fn stream(
        &self,
        base: &str,
        wire: WireProtocol,
        mut body: Value,
        events: &mpsc::Sender<ProviderEvent>,
        cancel: &CancellationToken,
        mut consume: impl FnMut(&str, &str) -> Result<()> + Send,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut url = Url::parse(base).map_err(|_| protocol("invalid provider API base URL"))?;
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(protocol(
                "provider base URL must not contain credentials, query, or fragment",
            ));
        }
        let local = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            return Err(protocol(
                "provider endpoints require HTTPS except loopback test/local servers",
            ));
        }
        let resource = match wire {
            WireProtocol::OpenAi => "responses",
            WireProtocol::Anthropic => "messages",
        };
        url.set_path(&format!(
            "{}/{}",
            url.path().trim_end_matches('/'),
            resource
        ));
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if matches!(wire, WireProtocol::Anthropic) {
            let mut key =
                HeaderValue::from_str(&self.key).map_err(|_| protocol("invalid API key header"))?;
            key.set_sensitive(true);
            headers.insert("x-api-key", key);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            let mut key = HeaderValue::from_str(&format!("Bearer {}", self.key))
                .map_err(|_| protocol("invalid API key header"))?;
            key.set_sensitive(true);
            headers.insert("authorization", key);
        }
        // The outer deadline includes backoff, response headers, and stream reads.
        let operation = async {
            let mut attempt = 0;
            let mut summary_fallback_used = false;
            let response = loop {
                let sent = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    sent = self.client.post(url.clone()).headers(headers.clone()).json(&body).send() => sent,
                };
                match sent {
                    Ok(mut received) if !received.status().is_success() => {
                        let status = received.status().as_u16();
                        let retry_after = received
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(parse_retry_after);
                        let mut bytes = Vec::new();
                        loop {
                            let next = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => return Err(Error::Cancelled),
                                next = tokio::time::timeout(Duration::from_secs(10), received.chunk()) => next,
                            };
                            match next {
                                Ok(Ok(Some(chunk))) => {
                                    let remaining = MAX_ERROR_BYTES.saturating_sub(bytes.len());
                                    bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                                    if bytes.len() >= MAX_ERROR_BYTES {
                                        break;
                                    }
                                }
                                _ => break,
                            }
                        }
                        // Compatibility retry is allowed only for an explicit
                        // HTTP rejection of the optional summary display field,
                        // before any successful stream has started. Preserve all
                        // reasoning effort, model, tool, and history settings.
                        if !summary_fallback_used
                            && omit_unsupported_summary(status, &bytes, &wire, &mut body)
                        {
                            summary_fallback_used = true;
                            continue;
                        }
                        let (kind, message) = classify(status, &bytes);
                        let retryable = matches!(
                            kind,
                            ProviderErrorKind::RateLimit | ProviderErrorKind::Transport
                        );
                        if retryable
                            && attempt < MAX_RETRIES
                            && retry_after.is_none_or(|wait| wait <= MAX_RETRY_WAIT)
                        {
                            self.backoff(attempt, retry_after, events, cancel).await?;
                            attempt += 1;
                            continue;
                        }
                        return Err(failure(kind, self.redact(&message)));
                    }
                    Ok(received) => {
                        break received;
                    }
                    Err(_) if attempt < MAX_RETRIES => {
                        self.backoff(attempt, None, events, cancel).await?;
                        attempt += 1;
                    }
                    Err(_) => {
                        return Err(failure(
                            ProviderErrorKind::Transport,
                            "connection failed after bounded retries",
                        ))
                    }
                }
            };
            if !response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                })
            {
                return Err(protocol("expected a text/event-stream response"));
            }
            let mut stream = response.bytes_stream();
            let mut decoder = Decoder::new();
            let mut total = 0usize;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => next,
                };
                match next {
                    Ok(Some(Ok(bytes))) => {
                        total = total
                            .checked_add(bytes.len())
                            .ok_or_else(|| protocol("stream size overflow"))?;
                        if total > MAX_STREAM_BYTES {
                            return Err(protocol("response stream exceeds the size limit"));
                        }
                        decoder.push(&bytes, &mut consume)?;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(_))) => {
                        return Err(failure(
                            ProviderErrorKind::Transport,
                            "response stream disconnected; partial output was not accepted",
                        ))
                    }
                    Err(_) => {
                        return Err(failure(
                            ProviderErrorKind::Transport,
                            "response stream idle timeout",
                        ))
                    }
                }
            }
            decoder.finish()
        };
        match tokio::time::timeout(TURN_TIMEOUT, operation).await {
            Ok(Err(Error::Provider { kind, message })) => Err(failure(kind, self.redact(&message))),
            Ok(result) => result,
            Err(_) => Err(failure(
                ProviderErrorKind::Transport,
                "provider turn deadline exceeded",
            )),
        }
    }

    fn redact(&self, message: &str) -> String {
        let message = if self.key.is_empty() {
            message.to_owned()
        } else {
            message.replace(&self.key, "[redacted]")
        };
        message.chars().take(512).collect()
    }

    async fn backoff(
        &self,
        attempt: u32,
        server: Option<Duration>,
        events: &mpsc::Sender<ProviderEvent>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis()
            % 200;
        let delay = server
            .unwrap_or_else(|| Duration::from_millis(250 * (1u64 << attempt) + u64::from(jitter)));
        let _ = events.try_send(ProviderEvent::Retrying {
            attempt: attempt + 1,
        });
        tokio::select! { biased; _ = cancel.cancelled() => Err(Error::Cancelled), _ = tokio::time::sleep(delay) => Ok(()) }
    }
}

/// Recognize only explicit provider complaints about the exact optional field.
/// Missing/ambiguous error metadata must fail normally instead of changing the
/// user's request. This never applies to errors inside an accepted SSE stream.
fn omit_unsupported_summary(
    status: u16,
    bytes: &[u8],
    wire: &WireProtocol,
    body: &mut Value,
) -> bool {
    if status != 400 {
        return false;
    }
    if !matches!(
        classify(status, bytes).0,
        ProviderErrorKind::Protocol | ProviderErrorKind::Unsupported
    ) {
        return false;
    }
    let (parent, field, expected) = match wire {
        WireProtocol::OpenAi => ("reasoning", "summary", "auto"),
        WireProtocol::Anthropic => ("thinking", "display", "summarized"),
    };
    if body[parent][field].as_str() != Some(expected) {
        return false;
    }
    let parsed: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let error = &parsed["error"];
    let path = format!("{parent}.{field}");
    let message = error["message"].as_str().unwrap_or("").to_ascii_lowercase();
    let targets_field = match error.get("param").filter(|value| !value.is_null()) {
        Some(parameter) => parameter.as_str() == Some(&path),
        None => {
            message.starts_with(&format!("{path}:"))
                || message.starts_with(&format!("{path} is not supported"))
                || message.starts_with(&format!("{path} is unsupported"))
                || [
                    "unsupported parameter",
                    "unknown parameter",
                    "unrecognized parameter",
                    "unsupported value",
                ]
                .iter()
                .any(|prefix| {
                    message.starts_with(&format!("{prefix}: '{path}'"))
                        || message.starts_with(&format!("{prefix}: \"{path}\""))
                })
        }
    };
    let explicitly_unsupported = matches!(
        error["code"].as_str(),
        Some("unsupported_parameter" | "unsupported_value")
    ) || [
        "not supported",
        "unsupported",
        "not permitted",
        "unknown parameter",
        "unrecognized parameter",
        "unrecognized field",
        "unknown field",
        "extra inputs",
    ]
    .iter()
    .any(|phrase| message.contains(phrase));
    if !targets_field || !explicitly_unsupported {
        return false;
    }
    body[parent].as_object_mut().unwrap().remove(field);
    true
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
}

pub(crate) fn classify(status: u16, bytes: &[u8]) -> (ProviderErrorKind, String) {
    let parsed: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let error = parsed.get("error").unwrap_or(&parsed);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("provider request failed");
    let lower = message.to_ascii_lowercase();
    let category = if matches!(status, 401 | 403) || kind == "authentication_error" {
        ProviderErrorKind::Authentication
    } else if kind == "insufficient_quota"
        || code.contains("quota")
        || code.contains("credit")
        || code.contains("spend_limit")
        || code.contains("usage_limit")
        || (status == 429
            && ["spend limit", "spend cap", "credit balance", "billing"]
                .iter()
                .any(|needle| lower.contains(needle)))
    {
        ProviderErrorKind::Quota
    } else if code.contains("context")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
    {
        ProviderErrorKind::Context
    } else if status == 429 || kind == "rate_limit_error" {
        ProviderErrorKind::RateLimit
    } else if status >= 500
        || status == 408
        || matches!(kind, "overloaded_error" | "api_error" | "server_error")
    {
        ProviderErrorKind::Transport
    } else if status == 404 {
        ProviderErrorKind::Unsupported
    } else {
        ProviderErrorKind::Protocol
    };
    (category, format!("HTTP {status}: {message}"))
}

pub(crate) fn stream_error(value: &Value) -> Error {
    let error = value.get("error").unwrap_or(value);
    let kind = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let status = match kind {
        "rate_limit_error" => 429,
        "overloaded_error" | "server_error" | "api_error" => 503,
        _ => 400,
    };
    let (kind, message) = classify(status, &serde_json::to_vec(value).unwrap_or_default());
    failure(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compatibility_retry_requires_an_explicit_exact_optional_field_rejection() {
        for (wire, body, error, expected) in [
            (
                WireProtocol::OpenAi,
                json!({"model":"gpt-5-nano", "reasoning":{"effort":"minimal", "summary":"auto"}}),
                json!({"error":{"param":"reasoning.summary", "code":"unsupported_value", "message":"Unsupported value: auto is not supported with this model"}}),
                json!({"model":"gpt-5-nano", "reasoning":{"effort":"minimal"}}),
            ),
            (
                WireProtocol::OpenAi,
                json!({"reasoning":{"effort":"low", "summary":"auto"}}),
                json!({"error":{"message":"Unsupported parameter: 'reasoning.summary' is not supported with this model."}}),
                json!({"reasoning":{"effort":"low"}}),
            ),
            (
                WireProtocol::Anthropic,
                json!({"thinking":{"type":"adaptive", "display":"summarized"}, "output_config":{"effort":"low"}}),
                json!({"error":{"type":"invalid_request_error", "message":"thinking.display: Extra inputs are not permitted"}}),
                json!({"thinking":{"type":"adaptive"}, "output_config":{"effort":"low"}}),
            ),
        ] {
            let mut body = body;
            let bytes = serde_json::to_vec(&error).unwrap();
            assert!(omit_unsupported_summary(400, &bytes, &wire, &mut body));
            assert_eq!(body, expected);
            assert!(!omit_unsupported_summary(400, &bytes, &wire, &mut body));
        }
        let original = json!({"reasoning":{"effort":"minimal", "summary":"auto"}});
        for (status, error) in [
            (
                400,
                json!({"error":{"param":"reasoning.effort", "code":"unsupported_value", "message":"Unsupported effort when reasoning.summary is enabled"}}),
            ),
            (
                400,
                json!({"error":{"param":"reasoning.summary", "message":"invalid request"}}),
            ),
            (
                400,
                json!({"error":{"message":"reasoning.summary is present, but reasoning.effort is unsupported"}}),
            ),
            (
                400,
                json!({"error":{"param":"reasoning.summary", "type":"authentication_error", "message":"unsupported"}}),
            ),
            (
                401,
                json!({"error":{"param":"reasoning.summary", "code":"unsupported_parameter"}}),
            ),
            (
                429,
                json!({"error":{"param":"reasoning.summary", "code":"unsupported_parameter"}}),
            ),
            (
                500,
                json!({"error":{"param":"reasoning.summary", "code":"unsupported_parameter"}}),
            ),
        ] {
            let mut body = original.clone();
            assert!(!omit_unsupported_summary(
                status,
                &serde_json::to_vec(&error).unwrap(),
                &WireProtocol::OpenAi,
                &mut body
            ));
            assert_eq!(body, original);
        }
    }
    #[test]
    fn quota_is_not_transient() {
        assert_eq!(
            classify(429, br#"{"error":{"code":"credit_balance_exhausted"}}"#).0,
            ProviderErrorKind::Quota
        );
        assert_eq!(classify(429, br#"{"error":{"type":"rate_limit_error","message":"usage tier spend cap reached"}}"#).0, ProviderErrorKind::Quota);
        assert_eq!(
            classify(429, br#"{"error":{"type":"rate_limit_error"}}"#).0,
            ProviderErrorKind::RateLimit
        );
    }
    #[test]
    fn retry_after_seconds_and_dates() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("invalid"), None);
    }
}
