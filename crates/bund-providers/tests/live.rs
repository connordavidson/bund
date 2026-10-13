//! Explicitly opt-in, billable smoke tests. Nothing in the regular test suite
//! reads credentials or calls a live API. Invoke an individual ignored test
//! only after setting its model and API-key environment variables.
use bund_core::{Message, ModelProfile, Provider, ProviderRequest, Role};
use bund_providers::{AnthropicProvider, OpenAiProvider};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("opt-in live test requires {name}; it was not set"))
}

async fn smoke(provider: &dyn Provider, model: String) {
    let mut profile = ModelProfile::new(provider.id(), model);
    profile.max_output_tokens = 256;
    let request = ProviderRequest {
        profile,
        instructions: "Reply with exactly OK. Keep your answer brief. No tools are available."
            .into(),
        messages: vec![Message::text(Role::User, "Reply OK.")],
        tools: vec![],
    };
    let (events, _receiver) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        provider.complete(request, events, cancel.clone()),
    )
    .await
    .inspect_err(|_| cancel.cancel())
    .expect("live provider did not complete within 60 seconds")
    .expect("live provider must return a valid completed response");
    assert!(
        !response.text.trim().is_empty(),
        "live response has no text"
    );
    assert!(response.tool_calls.is_empty(), "no tools were supplied");
    assert!(response.continuation.is_some(), "native state is required");
    assert!(response.usage.output_tokens > 0, "usage must be reported");
}

#[tokio::test]
#[ignore = "billable: requires BUND_LIVE_OPENAI_MODEL and OPENAI_API_KEY"]
async fn openai_live_smoke() {
    let model = required("BUND_LIVE_OPENAI_MODEL");
    let provider = OpenAiProvider::new(required("OPENAI_API_KEY"));
    smoke(&provider, model).await;
}

#[tokio::test]
#[ignore = "billable: requires BUND_LIVE_ANTHROPIC_MODEL and ANTHROPIC_API_KEY"]
async fn anthropic_live_smoke() {
    let model = required("BUND_LIVE_ANTHROPIC_MODEL");
    let provider = AnthropicProvider::new(required("ANTHROPIC_API_KEY"));
    smoke(&provider, model).await;
}
