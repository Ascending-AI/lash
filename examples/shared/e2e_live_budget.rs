//! Opt-in host policy for the paid E2E tier. Every attempted request reserves
//! its full upper bound before transport; failures retain the reservation.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use lash::direct::ProviderRouteIdentity;
use lash::provider::{
    GenerationRetryGuarantee, LlmRequest, LlmResponse, LlmTransportError, Provider,
    ProviderComponents, ProviderFailureKind, ProviderOptions, ProviderRequestBody,
    TransportRetryVerdict,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Budget {
    model: String,
    max_calls: usize,
    max_input_bytes: usize,
    max_output_tokens: usize,
    max_spend_usd: f64,
    /// Operator-supplied conservative maximum, including cache writes.
    input_usd_per_token: f64,
    output_usd_per_token: f64,
    receipts: PathBuf,
}

#[derive(Debug, Default)]
struct State {
    reserved_usd: f64,
    calls: Vec<Value>,
}

#[derive(Debug)]
struct Capped {
    inner: Box<dyn Provider>,
    budget: Arc<Budget>,
    state: Arc<Mutex<State>>,
}

pub(crate) fn install(components: ProviderComponents) -> Result<ProviderComponents> {
    let Some(path) = std::env::var_os("LASH_E2E_LIVE_BUDGET") else {
        return Ok(components);
    };
    let budget: Budget =
        serde_json::from_slice(&std::fs::read(path)?).context("paid E2E budget configuration")?;
    ensure!(
        !budget.model.is_empty()
            && budget.max_calls > 0
            && budget.max_input_bytes > 0
            && budget.max_output_tokens > 0,
        "paid E2E model and count/token bounds are required"
    );
    ensure!(
        [
            budget.max_spend_usd,
            budget.input_usd_per_token,
            budget.output_usd_per_token
        ]
        .iter()
        .all(|value| value.is_finite() && *value > 0.0),
        "paid E2E spend/rates must be finite and positive"
    );
    if let Some(parent) = budget.receipts.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let budget = Arc::new(budget);
    let state = Arc::new(Mutex::new(State::default()));
    Ok(components.map_provider(move |inner| {
        Box::new(Capped {
            inner,
            budget,
            state,
        })
    }))
}

fn refusal(message: impl Into<String>) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Quota)
        .with_code(lash::provider::FailureCode::provider("E2ELiveBudget"))
        .with_retry_verdict(TransportRetryVerdict::NotRetryable)
}

impl Capped {
    fn persist(&self, state: &State) -> std::result::Result<(), LlmTransportError> {
        let receipt = json!({"model":self.budget.model,"max_spend_usd":self.budget.max_spend_usd,
            "reserved_usd":state.reserved_usd,"max_calls":self.budget.max_calls,"calls":state.calls});
        std::fs::write(
            &self.budget.receipts,
            serde_json::to_vec_pretty(&receipt).map_err(|error| refusal(error.to_string()))?,
        )
        .map_err(|error| refusal(format!("write live usage receipt: {error}")))
    }
}

#[async_trait]
impl Provider for Capped {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.inner.route_identity(model)
    }
    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> Value {
        json!({"e2e_live_budget":true,"model":self.budget.model})
    }
    fn generation_retry_guarantee(
        &self,
        request: &LlmRequest,
        body: &ProviderRequestBody,
    ) -> GenerationRetryGuarantee {
        self.inner.generation_retry_guarantee(request, body)
    }
    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }
    async fn lower(
        &mut self,
        request: &LlmRequest,
    ) -> Result<ProviderRequestBody, LlmTransportError> {
        self.inner.lower(request).await
    }
    async fn send(
        &mut self,
        request: LlmRequest,
        body: &ProviderRequestBody,
    ) -> std::result::Result<LlmResponse, LlmTransportError> {
        let bytes = serde_json::to_vec(&request).map_err(|error| refusal(error.to_string()))?;
        let output = request
            .generation
            .output_token_cap_u64()
            .ok_or_else(|| refusal("live request has no output cap"))?;
        if request.model.wire_model() != self.budget.model
            || bytes.len() > self.budget.max_input_bytes
            || output > self.budget.max_output_tokens as u64
            || !request.attachments().is_empty()
        {
            return Err(refusal(
                "live request exceeds model/input/output/attachment bounds",
            ));
        }
        let reservation = bytes.len() as f64 * self.budget.input_usd_per_token
            + output as f64 * self.budget.output_usd_per_token;
        let ordinal = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| refusal("live budget state poisoned"))?;
            if state.calls.len() >= self.budget.max_calls
                || state.reserved_usd + reservation > self.budget.max_spend_usd
            {
                return Err(refusal("live request would exceed call or spend cap"));
            }
            state.reserved_usd += reservation;
            let ordinal = state.calls.len();
            state.calls.push(json!({"ordinal":ordinal+1,"request":request,"reservation_usd":reservation,"outcome":"entered"}));
            self.persist(&state)?;
            ordinal
        };
        let response = self.inner.send(request, body).await;
        let mut state = self
            .state
            .lock()
            .map_err(|_| refusal("live budget state poisoned"))?;
        let observed = match &response {
            Ok(response) => Some(response),
            Err(error) => error.partial_response.as_deref(),
        };
        state.calls[ordinal]["outcome"] = json!(if response.is_ok() {
            "completed"
        } else {
            "failed"
        });
        if let Some(observed) = observed {
            state.calls[ordinal]["served_model"] = json!(
                observed
                    .execution_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.served_model.as_deref())
            );
            state.calls[ordinal]["usage"] = json!(observed.usage);
            state.calls[ordinal]["provider_usage"] = json!(observed.provider_usage);
        }
        self.persist(&state)?;
        if response.is_ok()
            && observed.is_some_and(|response| {
                response.provider_usage.is_none()
                    || response
                        .execution_evidence
                        .as_ref()
                        .and_then(|evidence| evidence.served_model.as_ref())
                        .is_none()
            })
        {
            return Err(refusal("live response omitted served model or usage"));
        }
        response
    }
    async fn close(&self) -> std::result::Result<(), LlmTransportError> {
        self.inner.close().await
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
            budget: self.budget.clone(),
            state: self.state.clone(),
        })
    }
}
