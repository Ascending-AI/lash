mod chat;
pub mod codex;
mod common;
mod config;
mod driver;
mod provider;
#[cfg(test)]
mod provider_trace_tests;
mod reasoning;
mod request_work;
mod responses;
mod responses_output_evidence;
mod responses_shared;
mod responses_stream_event;
pub mod schema;
mod support;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

pub use codex::CodexProvider;
pub use common::{OPENAI_BASE_URL, OPENROUTER_BASE_URL};
pub use config::{
    OpenAiCompat, OpenAiCompatMaxTokensField, OpenAiCompatibleProvider, OpenAiProvider,
    OpenAiWireConfig, ProviderRoutingPrefs,
};
pub use driver::CompletionEndpoint;
pub use reasoning::{
    ReasoningEncodeError, ReasoningWireEncoder, ReasoningWireFormat, ReasoningWireIntent,
};

#[cfg(test)]
mod attachment_capability_fixture;
#[cfg(test)]
pub(crate) use attachment_capability_fixture::attachment_test_capability;
