//! The attempt loop fills an admitted template's slots afresh on every
//! attempt (ADR 0133 §6, ADR 0135 §4, FIG-5445): a transient delivery fault
//! is an unsent, retried attempt, and a provider's definite rejection of a
//! delivery forgets that delivery before the next attempt delivers again.

use super::*;
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryContext, DeliverySecret, ProviderAccepts,
};
use lash_sansio::llm::types::SlotCodec;

/// The route the slot provider serves.
fn slot_route() -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint("slots", "https://slots.example/v1", "model")
}

/// Rejects the first delivery it is sent as an expired file would be, then
/// answers; records every wire.
#[derive(Clone, Debug)]
struct RejectOnceProvider {
    options: ProviderOptions,
    wires: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Provider for RejectOnceProvider {
    fn kind(&self) -> &'static str {
        "slots"
    }

    fn route_identity(&self, _model: &str) -> ProviderRouteIdentity {
        slot_route()
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn send(
        &mut self,
        body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        let first = {
            let mut wires = self.wires.lock_recover();
            wires.push(body.wire());
            wires.len() == 1
        };
        if first {
            let mut error = LlmTransportError::new("the delivered URL could not be fetched")
                .with_kind(ProviderFailureKind::Validation)
                .with_rejected_slots(vec![0]);
            error.http_status = Some(400);
            return Err(error);
        }
        Ok(bare_ok_response())
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// Fails its first delivery transiently, then signs a fresh URL for each.
#[derive(Default)]
struct FlakySigner {
    calls: AtomicUsize,
    invalidated: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SlotDeliveries for FlakySigner {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 1 {
            return Err(AttachmentDeliveryError::Unavailable {
                retryable: true,
                message: "the store is briefly unreachable".to_owned(),
            });
        }
        Ok(slots
            .iter()
            .map(|_| {
                Arc::new(Delivery::Url {
                    url: DeliverySecret::new(format!("https://store.example/blob?sig={call}")),
                    valid_until_ms: Some(ctx.valid_through_ms),
                })
            })
            .collect())
    }

    async fn invalidate(
        &self,
        _reference: &lash_sansio::AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        if let Some(secret) = rejected.secret() {
            self.invalidated
                .lock_recover()
                .push(secret.expose().to_owned());
        }
        Ok(())
    }
}

fn slot_template() -> RecordedRequestTemplate {
    let mut builder = RecordedRequestTemplate::builder(slot_route(), false, None);
    builder
        .literal("{\"image\":")
        .attachment(AttachmentSlot {
            reference: lash_sansio::AttachmentRef {
                id: lash_sansio::AttachmentId::parse("cd".repeat(32)).expect("a digest id"),
                media_type: lash_sansio::MediaType::parse("image/png").expect("a media type"),
                byte_len: 3,
                type_metadata: None,
                label: None,
            },
            position: AttachmentPosition::Message,
            accepts: ProviderAccepts {
                bytes: false,
                url: true,
                provider_file: None,
            },
            codec: SlotCodec {
                name: "lash.canonical".into(),
                revision: 1,
            },
        })
        .literal("}");
    builder.finish().expect("a valid template")
}

#[tokio::test]
async fn every_attempt_delivers_afresh_and_unsent_or_rejected_deliveries_are_retried() {
    let wires = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = RejectOnceProvider {
        options: ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(Some(3))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..Default::default()
        },
        wires: Arc::clone(&wires),
    };
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(provider)));
    let signer = FlakySigner::default();
    let mut request = empty_request();
    let sideband = handle.prepare_completion(&mut request);
    let completion = handle
        .complete_prepared(
            ResponseContext::of_request(&request),
            &Arc::new(slot_template()),
            &signer,
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &lash_trace::telemetry::metrics::TelemetryMetrics::default(),
            None,
            crate::provider::handle::ModelCallBounds::default(),
        )
        .await
        .expect("the third attempt answers");

    let attempts = &completion.call_record.attempts;
    assert_eq!(attempts.len(), 3, "{attempts:?}");
    let unsent = attempts[0]
        .error
        .as_ref()
        .expect("the first attempt failed");
    assert_eq!(
        unsent.code,
        Some(FailureCode::lash(
            TurnFailureCode::AttachmentDeliveryUnavailable
        ))
    );
    assert_eq!(attempts[0].protocol_position, ProtocolPosition::NoResponse);
    assert_eq!(
        *wires.lock_recover(),
        [
            "{\"image\":{\"url\":\"https://store.example/blob?sig=2\"}}",
            "{\"image\":{\"url\":\"https://store.example/blob?sig=3\"}}",
        ],
        "the provider is sent each attempt's own delivery, never the unsent one's"
    );
    assert_eq!(
        *signer.invalidated.lock_recover(),
        ["https://store.example/blob?sig=2"],
        "the rejected delivery is forgotten before the next attempt"
    );
}
