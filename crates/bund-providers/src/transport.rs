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
        body: Value,
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
            let mut response = None;
            for attempt in 0..=MAX_RETRIES {
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
                            continue;
                        }
                        return Err(failure(kind, self.redact(&message)));
                    }
                    Ok(received) => {
                        response = Some(received);
                        break;
                    }
                    Err(_) if attempt < MAX_RETRIES => {
                        self.backoff(attempt, None, events, cancel).await?;
                    }
                    Err(_) => {
                        return Err(failure(
                            ProviderErrorKind::Transport,
                            "connection failed after bounded retries",
                        ))
                    }
                }
            }
            let response = response.ok_or_else(|| protocol("request did not return a response"))?;
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
