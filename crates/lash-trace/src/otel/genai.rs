//! GenAI instruments observe the very projection that supplies span values,
//! before sampling, export selection or host enrichment can change it.
use super::*;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use registry::{COUNT_BOUNDARIES, DURATION_BOUNDARIES, Metric as M};

#[derive(Clone)]
pub(super) struct GenAiMetrics {
    inference: Histogram<f64>,
    first_chunk: Histogram<f64>,
    tokens: [(A, Counter<u64>); 5],
    operation_input: Histogram<u64>,
    operation_output: Histogram<u64>,
    agent: Histogram<f64>,
    inference_calls: Histogram<u64>,
    tool_calls: Histogram<u64>,
    tool: Histogram<f64>,
    workflow: Histogram<f64>,
}

impl GenAiMetrics {
    pub(super) fn new(meter: Meter) -> Self {
        Self {
            inference: seconds(&meter, M::InferenceDuration),
            first_chunk: seconds(&meter, M::FirstChunk),
            tokens: [
                (A::InputTokens, M::InputTokens),
                (A::OutputTokens, M::OutputTokens),
                (A::CacheReadTokens, M::CacheReadTokens),
                (A::CacheWriteTokens, M::CacheWriteTokens),
                (A::ReasoningTokens, M::ReasoningTokens),
            ]
            .map(|(key, metric)| {
                let def = metric.definition();
                (key, meter.u64_counter(def.name).with_unit(def.unit).build())
            }),
            operation_input: count(&meter, M::OperationInputTokens),
            operation_output: count(&meter, M::OperationOutputTokens),
            agent: seconds(&meter, M::AgentDuration),
            inference_calls: count(&meter, M::AgentInferenceCalls),
            tool_calls: count(&meter, M::AgentToolCalls),
            tool: seconds(&meter, M::ToolDuration),
            workflow: seconds(&meter, M::WorkflowDuration),
        }
    }

    pub(super) fn record(&self, record: &TraceRecord, position: &TracePosition) {
        let Some(projection) = Projection::for_record(record, position) else {
            return;
        };
        let mut attributes = Vec::new();
        projection.attributes(record, &mut attributes);
        let keys: &[A] = match position.kind {
            TraceSpanKind::Model => &[
                A::OperationName,
                A::GenAiProviderName,
                A::RequestModel,
                A::ResponseModel,
                A::ErrorType,
            ],
            TraceSpanKind::Agent => &[A::OperationName, A::ErrorType],
            TraceSpanKind::Tool | TraceSpanKind::Cell => &[A::ToolName, A::ErrorType],
            TraceSpanKind::Workflow => &[A::WorkflowName, A::ErrorType],
            _ => return,
        };
        let labels: Vec<_> = attributes
            .iter()
            .filter(|attribute| {
                keys.iter()
                    .any(|key| key.definition().key == attribute.key.as_str())
            })
            .cloned()
            .collect();
        let duration = position
            .ended_at
            .unwrap_or(record.timestamp)
            .duration_since(position.started_at)
            .as_secs_f64();
        match position.kind {
            TraceSpanKind::Model => {
                self.inference.record(duration, &labels);
                let (model, summary) = match &record.event {
                    TraceEvent::LlmCallCompleted {
                        model,
                        stream_summary,
                        ..
                    }
                    | TraceEvent::LlmCallFailed {
                        model,
                        stream_summary,
                        ..
                    } => (model, stream_summary),
                    _ => return,
                };
                if model.as_ref().is_some_and(|model| model.stream)
                    && let Some(first) = summary
                        .as_ref()
                        .and_then(|s| s.get("first_chunk_latency_ms"))
                        .and_then(serde_json::Value::as_u64)
                {
                    self.first_chunk.record(
                        std::time::Duration::from_millis(first).as_secs_f64(),
                        &labels,
                    );
                }
                for (key, counter) in &self.tokens {
                    if let Some(value) = token(&attributes, *key) {
                        counter.add(value, &labels);
                    }
                }
                if let Some(value) = token(&attributes, A::InputTokens) {
                    self.operation_input.record(value, &labels);
                }
                if let Some(value) = token(&attributes, A::OutputTokens) {
                    self.operation_output.record(value, &labels);
                }
            }
            TraceSpanKind::Agent => {
                self.agent.record(duration, &labels);
                if let TraceEvent::TurnCompleted {
                    inference_calls,
                    tool_calls,
                    ..
                } = &record.event
                {
                    if let Some(calls) = inference_calls {
                        self.inference_calls.record(u64::from(*calls), &labels);
                    }
                    if let Some(calls) = tool_calls {
                        self.tool_calls.record(*calls, &labels);
                    }
                }
            }
            TraceSpanKind::Tool | TraceSpanKind::Cell => self.tool.record(duration, &labels),
            TraceSpanKind::Workflow => self.workflow.record(duration, &labels),
            _ => {}
        }
    }
}

fn token(attributes: &[KeyValue], key: A) -> Option<u64> {
    attributes.iter().find_map(|attribute| {
        if attribute.key.as_str() == key.definition().key
            && let opentelemetry::Value::I64(value) = attribute.value
        {
            u64::try_from(value).ok()
        } else {
            None
        }
    })
}

fn seconds(meter: &Meter, metric: M) -> Histogram<f64> {
    let def = metric.definition();
    meter
        .f64_histogram(def.name)
        .with_unit(def.unit)
        .with_boundaries(DURATION_BOUNDARIES.to_vec())
        .build()
}
fn count(meter: &Meter, metric: M) -> Histogram<u64> {
    let def = metric.definition();
    meter
        .u64_histogram(def.name)
        .with_unit(def.unit)
        .with_boundaries(COUNT_BOUNDARIES.to_vec())
        .build()
}
