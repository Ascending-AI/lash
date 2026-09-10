use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use lash_core::{
    ProviderFailureKind, facade_support::LlmTransportError, llm::transport::TransportRetryVerdict,
};
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport, run_with_timeout,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

mod transport;
mod wire_script;

#[cfg(test)]
mod tests;

pub use transport::{
    ProviderWireTimelineEntry, ScriptedLlmHttpExchange, ScriptedLlmHttpRequestExchange,
    ScriptedLlmHttpResponseExchange, ScriptedLlmHttpTransport, ScriptedProviderEventRelease,
    ScriptedTransportSchedule,
};
use transport::{failing_timeline_event_index, script_validation_error};
use wire_script::{BodyPlan, ScriptedResponsePlan, StreamStep};
pub use wire_script::{
    HeaderMatcher, JsonMatcher, PROVIDER_WIRE_SCRIPT_SCHEMA, ProviderWireChunkPayload,
    ProviderWireEndpoint, ProviderWireEvent, ProviderWireHeader, ProviderWireProvenance,
    ProviderWireProvenanceKind, ProviderWireRequestMatch, ProviderWireScript,
};
