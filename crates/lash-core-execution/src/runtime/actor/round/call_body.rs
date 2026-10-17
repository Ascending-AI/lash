//! The archival component of a recorded call (FIG-6171). The live outcome
//! and its consuming turn share this exact node. Execution pins and model
//! presentation remain outside it and may be reclaimed independently.

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct CallBody {
    pub record: crate::ToolCallRecord,
}

pub(super) struct StoredBody {
    pub(super) record: crate::ToolCallRecord,
    pub(super) answer: Option<NodeId>,
}

fn encode_body(
    call_id: &ToolCallId,
    provider_call_id: &Option<String>,
    tool: &str,
    args: &serde_json::Value,
    output: &ToolCallOutput,
    answer: Option<NodeId>,
    out: &mut TokenWriter,
) {
    out.seq(5);
    out.text(call_id.as_str());
    provider_call_id.encode(out);
    out.text(tool);
    args.encode(out);
    encode_output(
        output,
        &RefAnswer {
            node: Cell::new(answer),
        },
        out,
    );
}

impl TokenCodec for StoredBody {
    fn encode(&self, out: &mut TokenWriter) {
        encode_body(
            &self.record.call_id,
            &self.record.provider_call_id,
            &self.record.tool,
            &self.record.args,
            &self.record.output,
            self.answer,
            out,
        );
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        let call_id = decode_call_id(input)?;
        let provider_call_id = Option::decode(input)?;
        let tool = input.text()?.to_owned();
        let args = serde_json::Value::decode(input)?;
        let answer = RefAnswer {
            node: Cell::new(None),
        };
        let output = decode_output(input, &answer)?;
        Ok(Self {
            record: crate::ToolCallRecord {
                call_id,
                provider_call_id,
                tool,
                args,
                output,
            },
            answer: answer.node.get(),
        })
    }
}

impl CallBody {
    /// Stage the one archival body, with the success value in a shared child.
    ///
    /// # Errors
    /// Any staging refusal.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        Self::stage_fields(
            staging,
            &self.record.call_id,
            &self.record.provider_call_id,
            &self.record.tool,
            &self.record.args,
            &self.record.output,
        )
    }

    pub(super) fn stage_fields(
        staging: &mut GraphStaging,
        call_id: &ToolCallId,
        provider_call_id: &Option<String>,
        tool: &str,
        args: &serde_json::Value,
        output: &ToolCallOutput,
    ) -> Result<NodeId, GraphRefusal> {
        let answer = match &output.outcome {
            crate::ToolCallOutcome::Success(value) => {
                let mut body = TokenWriter::new();
                body.sum(
                    lash_protocol_types::tool_output::graph::FROZEN_TOOL_VALUE,
                    1,
                );
                value.encode(&mut body);
                Some(staging.encode(
                    NodeKind::FROZEN_VALUE,
                    TypedBody::logical(&body.into_bytes()),
                )?)
            }
            crate::ToolCallOutcome::Failure(_) | crate::ToolCallOutcome::Cancelled(_) => None,
        };
        let mut body = TokenWriter::new();
        encode_body(
            call_id,
            provider_call_id,
            tool,
            args,
            output,
            answer,
            &mut body,
        );
        Ok(staging.encode(NodeKind::CALL_BODY, TypedBody::logical(&body.into_bytes()))?)
    }

    /// Load an archival body without the admission or model presentation.
    ///
    /// # Errors
    /// A decoding refusal, or a successful outcome lacking its answer.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let StoredBody { mut record, answer } = decoder.decode(id, NodeKind::CALL_BODY)?;
        match (&mut record.output.outcome, answer) {
            (crate::ToolCallOutcome::Success(value), Some(answer)) => {
                let FrozenToolValue(loaded) = decoder.decode(answer, NodeKind::FROZEN_VALUE)?;
                *value = loaded;
            }
            (crate::ToolCallOutcome::Success(_), None) => {
                return Err(GraphRefusal::InvalidBody {
                    kind: NodeKind::CALL_BODY.0,
                    reason: "a successful call body has no answer".to_owned(),
                });
            }
            _ => {}
        }
        Ok(Self { record })
    }
}
