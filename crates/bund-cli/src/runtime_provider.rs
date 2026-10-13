//! Resolve credentials only when a model request begins, keeping local commands
//! and the idle UI independent of Keychain availability.

use async_trait::async_trait;
use bund_core::{
    Error, Provider, ProviderErrorKind, ProviderEvent, ProviderRequest, ProviderResponse, Result,
};
use bund_providers::{AnthropicProvider, OpenAiProvider};
use std::sync::Arc;
use tokio::sync::{mpsc, OnceCell};
use tokio_util::sync::CancellationToken;

type CredentialLoader = dyn Fn(&str, bool) -> anyhow::Result<Option<String>> + Send + Sync;

pub struct RuntimeProvider {
    provider: &'static str,
    allow_keychain: bool,
    credentials: Arc<CredentialLoader>,
    ready: OnceCell<Arc<dyn Provider>>,
}

impl RuntimeProvider {
    pub fn new(provider: &'static str, allow_keychain: bool) -> Self {
        Self {
            provider,
            allow_keychain,
            credentials: Arc::new(crate::auth::credential),
            ready: OnceCell::new(),
        }
    }

    async fn resolve(&self, cancel: &CancellationToken) -> Result<Arc<dyn Provider>> {
        let (label, variable) = match self.provider {
            "openai" => ("OpenAI", "OPENAI_API_KEY"),
            "anthropic" => ("Anthropic", "ANTHROPIC_API_KEY"),
            _ => {
                return Err(Error::Provider {
                    kind: ProviderErrorKind::Unsupported,
                    message: format!("unsupported provider: {}", self.provider),
                });
            }
        };
        let ready = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            ready = self.ready.get_or_try_init(|| async {
                let loader = self.credentials.clone();
                let provider = self.provider;
                let allow_keychain = self.allow_keychain;
                // A platform credential lookup can block. Cancelling this future
                // cannot stop that read, but its detached result cannot initialize
                // the provider or start a model request.
                let loaded = tokio::task::spawn_blocking(move || loader(provider, allow_keychain))
                    .await;
                let key = match loaded {
                    Ok(Ok(key)) => key.filter(|key| !key.trim().is_empty()),
                    _ => {
                        return Err(Error::Provider {
                            kind: ProviderErrorKind::Authentication,
                            // Do not include arbitrary loader errors: they may
                            // contain credentials or other sensitive values.
                            message: format!(
                                "{label} API key could not be read. Check credential access, run \
                                 `bund auth login --provider {provider}`, or set {variable} before launching Bund."
                            ),
                        });
                    }
                };
                let key = key.ok_or_else(|| Error::Provider {
                    kind: ProviderErrorKind::Authentication,
                    message: format!(
                        "{label} API key is not configured. Run `bund auth login --provider {provider}`, \
                         then retry, or set {variable} before launching Bund."
                    ),
                })?;
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let concrete: Arc<dyn Provider> = match provider {
                    "openai" => Arc::new(OpenAiProvider::new(key)),
                    "anthropic" => Arc::new(AnthropicProvider::new(key)),
                    _ => unreachable!("provider was validated before lookup"),
                };
                Ok(concrete)
            }) => ready?,
        };
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(ready.clone())
    }
}

#[async_trait]
impl Provider for RuntimeProvider {
    fn id(&self) -> &str {
        self.provider
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse> {
        if request.profile.provider != self.provider {
            return Err(Error::Provider {
                kind: ProviderErrorKind::Unsupported,
                message: "provider/profile mismatch".into(),
            });
        }
        let provider = self.resolve(&cancel).await?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = provider.complete(request, events, cancel.clone()) => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bund_core::{ModelProfile, Usage};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };

    fn with_loader(
        provider: &'static str,
        allow_keychain: bool,
        credentials: impl Fn(&str, bool) -> anyhow::Result<Option<String>> + Send + Sync + 'static,
    ) -> RuntimeProvider {
        RuntimeProvider {
            provider,
            allow_keychain,
            credentials: Arc::new(credentials),
            ready: OnceCell::new(),
        }
    }

    fn request(provider: &str) -> ProviderRequest {
        ProviderRequest {
            profile: ModelProfile::new(provider, "fixture-model"),
            instructions: "fixture instructions".into(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    fn authentication_error(result: Result<Arc<dyn Provider>>) -> String {
        match result {
            Err(Error::Provider {
                kind: ProviderErrorKind::Authentication,
                message,
            }) => message,
            _ => panic!("expected an authentication error"),
        }
    }

    #[tokio::test]
    async fn lookup_is_lazy_selected_and_success_is_cached() {
        for (selected, allow_keychain) in [("openai", false), ("anthropic", true)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = calls.clone();
            let runtime = with_loader(selected, allow_keychain, move |provider, allow| {
                assert_eq!(provider, selected);
                assert_eq!(allow, allow_keychain);
                recorded.fetch_add(1, Ordering::SeqCst);
                Ok(Some("injected-fixture-key".into()))
            });
            assert_eq!(runtime.id(), selected);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let first = runtime.resolve(&CancellationToken::new()).await.unwrap();
            let second = runtime.resolve(&CancellationToken::new()).await.unwrap();
            assert_eq!(first.id(), selected);
            assert!(Arc::ptr_eq(&first, &second));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn missing_and_blank_credentials_are_not_cached() {
        for (selected, label, variable) in [
            ("openai", "OpenAI", "OPENAI_API_KEY"),
            ("anthropic", "Anthropic", "ANTHROPIC_API_KEY"),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = calls.clone();
            let runtime = with_loader(selected, false, move |_, _| {
                Ok(match recorded.fetch_add(1, Ordering::SeqCst) {
                    0 => None,
                    1 => Some("  \t".into()),
                    _ => Some("injected-fixture-key".into()),
                })
            });
            for _ in 0..2 {
                let message =
                    authentication_error(runtime.resolve(&CancellationToken::new()).await);
                assert_eq!(message, format!(
                    "{label} API key is not configured. Run `bund auth login --provider {selected}`, then retry, or set {variable} before launching Bund."
                ));
                assert!(runtime.ready.get().is_none());
            }
            assert_eq!(
                runtime
                    .resolve(&CancellationToken::new())
                    .await
                    .unwrap()
                    .id(),
                selected
            );
            assert_eq!(calls.load(Ordering::SeqCst), 3);
        }
    }

    #[tokio::test]
    async fn lookup_errors_are_redacted_and_retryable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = calls.clone();
        let runtime = with_loader("openai", false, move |_, _| {
            if recorded.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("secret-fixture-value");
            }
            Ok(Some("injected-fixture-key".into()))
        });
        let message = authentication_error(runtime.resolve(&CancellationToken::new()).await);
        assert!(message.contains("OpenAI API key could not be read"));
        assert!(!message.contains("secret-fixture-value"));
        assert!(runtime.ready.get().is_none());
        assert!(runtime.resolve(&CancellationToken::new()).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unsupported_provider_and_mismatched_profile_never_load_credentials() {
        for (selected, requested) in [("unknown", "unknown"), ("openai", "anthropic")] {
            let runtime = with_loader(selected, true, |_, _| panic!("must not load credentials"));
            let (events, _rx) = mpsc::channel(1);
            let result = runtime
                .complete(request(requested), events, CancellationToken::new())
                .await;
            assert!(matches!(
                result,
                Err(Error::Provider {
                    kind: ProviderErrorKind::Unsupported,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn cancellation_before_lookup_never_loads_credentials() {
        let runtime = with_loader("openai", true, |_, _| panic!("must not load credentials"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (events, _rx) = mpsc::channel(1);
        assert!(matches!(
            runtime.complete(request("openai"), events, cancel).await,
            Err(Error::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancellation_during_blocking_lookup_returns_before_it_finishes_and_can_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = calls.clone();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let (finished, finished_rx) = tokio::sync::oneshot::channel();
        let finished = Mutex::new(Some(finished));
        let runtime = Arc::new(with_loader("openai", true, move |_, _| {
            if recorded.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))?;
                let _ = finished.lock().unwrap().take().unwrap().send(());
            }
            Ok(Some("injected-fixture-key".into()))
        }));
        let cancel = CancellationToken::new();
        let task_runtime = runtime.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { task_runtime.resolve(&task_cancel).await });
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("cancellation must not wait for the blocking lookup")
            .unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(runtime.ready.get().is_none());
        release.send(()).unwrap();
        finished_rx.await.unwrap();
        assert!(runtime.ready.get().is_none());
        assert!(runtime.resolve(&CancellationToken::new()).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    struct RecordingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            "openai"
        }

        async fn complete(
            &self,
            request: ProviderRequest,
            events: mpsc::Sender<ProviderEvent>,
            cancel: CancellationToken,
        ) -> Result<ProviderResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.profile.model, "fixture-model");
            assert_eq!(request.instructions, "fixture instructions");
            assert!(!cancel.is_cancelled());
            events
                .send(ProviderEvent::TextDelta("fixture".into()))
                .await
                .unwrap();
            Ok(ProviderResponse {
                text: "fixture".into(),
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
                continuation: None,
                usage: Usage {
                    input_tokens: 13,
                    output_tokens: 2,
                    cached_tokens: 0,
                },
            })
        }
    }

    #[tokio::test]
    async fn forwards_complete_and_events_but_never_starts_after_cancellation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = with_loader("openai", false, |_, _| panic!("already initialized"));
        assert!(runtime
            .ready
            .set(Arc::new(RecordingProvider {
                calls: calls.clone()
            }))
            .is_ok());
        let (events, mut rx) = mpsc::channel(1);
        let response = runtime
            .complete(request("openai"), events, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.text, "fixture");
        assert_eq!(response.usage.input_tokens, 13);
        assert!(
            matches!(rx.recv().await, Some(ProviderEvent::TextDelta(text)) if text == "fixture")
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (events, _rx) = mpsc::channel(1);
        assert!(matches!(
            runtime.complete(request("openai"), events, cancel).await,
            Err(Error::Cancelled)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
