//! Real-host correctness controller. Adapters share identities, barriers and receipts.
pub mod case;
pub mod cluster;
pub mod control;
pub mod evidence;
pub mod host;
pub mod host_adapters;
pub mod otlp;
pub mod provider;
pub mod provider_http;

/// Object-safe asynchronous adapter operation.
pub type Step<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>>;
