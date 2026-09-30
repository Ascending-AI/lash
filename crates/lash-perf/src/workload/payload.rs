use super::{Generator, OperationId, ToolCallPlan, TurnPlan};
use anyhow::{Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rand_chacha::rand_core::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const VOCABULARY: &[&str] = &[
    "oak", "elm", "maple", "river", "stone", "orbit", "lambda", "λ", "ø",
];
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

pub struct SyntheticPayloads {
    pub prompt: String,
    pub input: Value,
    pub tool_schema: Value,
    pub tool_arguments: Vec<Value>,
    pub tool_results: Vec<Value>,
    pub attachments: Vec<SyntheticAttachment>,
}

#[derive(Debug, Clone)]
pub struct SyntheticAttachment {
    pub blob_key: String,
    pub owner_actors: Vec<u64>,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

impl SyntheticAttachment {
    /// Verify the container, CRCs, decoded seeded pixel, length and content digest.
    pub fn verify(&self, expected_bytes: u32) -> Result<()> {
        ensure!(self.media_type == "image/png", "attachment MIME mismatch");
        ensure!(
            self.bytes.len() == expected_bytes as usize,
            "attachment length mismatch"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&self.bytes)) == self.sha256,
            "attachment digest mismatch"
        );
        ensure!(
            self.bytes.starts_with(PNG_SIGNATURE),
            "invalid PNG signature"
        );
        let mut remaining = &self.bytes[8..];
        let mut kinds = Vec::new();
        while !remaining.is_empty() {
            ensure!(remaining.len() >= 12, "truncated PNG chunk");
            let length = u32::from_be_bytes(remaining[..4].try_into()?) as usize;
            ensure!(length <= remaining.len() - 12, "truncated PNG data");
            let kind = &remaining[4..8];
            let data = &remaining[8..8 + length];
            let crc = u32::from_be_bytes(remaining[8 + length..12 + length].try_into()?);
            ensure!(
                crc32fast::hash(&remaining[4..8 + length]) == crc,
                "invalid PNG CRC"
            );
            match kind {
                b"IHDR" => ensure!(
                    data == [0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0],
                    "invalid PNG header"
                ),
                b"IDAT" => {
                    let mut decoded = Vec::new();
                    ZlibDecoder::new(data).take(6).read_to_end(&mut decoded)?;
                    ensure!(
                        decoded.len() == 5 && decoded[0] == 0,
                        "invalid PNG scanline"
                    );
                }
                b"laSh" => {}
                b"IEND" => ensure!(data.is_empty(), "invalid PNG end"),
                _ => anyhow::bail!("unexpected PNG chunk"),
            }
            kinds.push(kind);
            remaining = &remaining[12 + length..];
        }
        ensure!(
            kinds == [b"IHDR", b"IDAT", b"laSh", b"IEND"],
            "invalid PNG chunk order"
        );
        Ok(())
    }
}

impl Generator<'_> {
    pub fn text(&self, actor: u64, ordinal: u64, purpose: &str, bytes: u32) -> String {
        let mut rng = self.stream(actor, ordinal, purpose);
        let mut text = String::with_capacity(bytes as usize);
        while text.len() < bytes as usize {
            let word = VOCABULARY[rng.next_u32() as usize % VOCABULARY.len()];
            if text.len() + word.len() <= bytes as usize {
                text.push_str(word);
            } else {
                text.push('x');
            }
            if text.len() < bytes as usize {
                text.push(' ');
            }
        }
        text
    }

    pub(super) fn tool_word(&self, actor: u64, ordinal: u64) -> &'static str {
        let words = ["oak ", "elm ", "fir ", "ash "];
        words[self
            .stream(actor, ordinal, "cell-tool-vocabulary")
            .next_u32() as usize
            % words.len()]
    }

    /// The call's arguments: its key, the result size and callback delay the
    /// synthetic tool must honour, and vocabulary padding to the sampled
    /// serialized size.
    pub fn tool_argument(&self, actor: u64, ordinal: u64, call: &ToolCallPlan) -> Result<Value> {
        let mut argument = json!({
            "record": {
                "kind": "synthetic",
                "key": call.idempotency_key,
                "result_bytes": call.result_bytes,
                "callback_ms": call.callback_ms,
            },
            "payload": "",
        });
        let padding = tool_argument_padding(call)?;
        let word = self.tool_word(actor, ordinal);
        argument["payload"] = word.repeat(padding.div_ceil(word.len()))[..padding].into();
        Ok(argument)
    }

    pub fn record(&self, actor: u64, ordinal: u64, purpose: &str, bytes: u32) -> Result<Value> {
        let mut record = json!({"record": {"kind": "synthetic"}, "payload": ""});
        let overhead = serde_json::to_vec(&record)?.len();
        ensure!(bytes as usize >= overhead, "JSON byte target is too small");
        record["payload"] = self
            .text(actor, ordinal, purpose, bytes - overhead as u32)
            .into();
        ensure!(
            serde_json::to_vec(&record)?.len() == bytes as usize,
            "JSON size mismatch"
        );
        Ok(record)
    }

    pub fn png(&self, actor: u64, ordinal: u64, purpose: &str, bytes: u32) -> Result<Vec<u8>> {
        let mut rng = self.stream(actor, ordinal, purpose);
        let mut pixel = [0u8; 5];
        rng.fill_bytes(&mut pixel[1..]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&pixel)?;
        let image = encoder.finish()?;
        let mut png = PNG_SIGNATURE.to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        chunk(&mut png, b"IDAT", &image);
        let overhead = png.len() + 24;
        ensure!(bytes as usize >= overhead, "PNG byte target is too small");
        let mut padding = vec![0; bytes as usize - overhead];
        rng.fill_bytes(&mut padding);
        chunk(&mut png, b"laSh", &padding);
        chunk(&mut png, b"IEND", &[]);
        Ok(png)
    }

    pub fn materialize(&self, plan: &TurnPlan) -> Result<SyntheticPayloads> {
        ensure!(
            plan.operation.run == self.operation(0, 0).run,
            "plan belongs to a different run"
        );
        let id = &plan.operation;
        let mut tool_arguments = Vec::new();
        let mut tool_results = Vec::new();
        for calls in &plan.tool_batches {
            for call in calls {
                tool_arguments.push(self.tool_argument(id.actor, id.ordinal, call)?);
                tool_results.push(self.tool_result(&call.idempotency_key, call.result_bytes)?);
            }
        }
        let attachments = (0..plan.attachments.len())
            .map(|index| self.attachment(plan, index))
            .collect::<Result<Vec<_>>>()?;
        Ok(SyntheticPayloads {
            prompt: self.text(id.actor, id.ordinal, "prompt", plan.prompt_bytes),
            input: self.record(id.actor, id.ordinal, "input", plan.input_bytes)?,
            tool_schema: tool_schema(),
            tool_arguments,
            tool_results,
            attachments,
        })
    }

    /// Blob `index` of `plan`: generated from its first owner's stream, so the
    /// adjacent actor that shares it regenerates identical bytes.
    pub fn attachment(&self, plan: &TurnPlan, index: usize) -> Result<SyntheticAttachment> {
        let attachment = plan
            .attachments
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("plan {} has no blob {index}", plan.operation.key()))?;
        let actor = attachment
            .owner_actors
            .first()
            .copied()
            .unwrap_or(plan.operation.actor);
        let bytes = self.png(
            actor,
            plan.operation.ordinal,
            &format!("blob/{index}"),
            attachment.bytes,
        )?;
        let generated = SyntheticAttachment {
            blob_key: attachment.blob_key.clone(),
            owner_actors: attachment.owner_actors.clone(),
            media_type: "image/png".into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        };
        generated.verify(attachment.bytes)?;
        Ok(generated)
    }

    /// The synthetic result for the tool call keyed `key`
    /// (`run/actor/ordinal/tool/batch/index`), regenerated identically by the
    /// tool that returns it and by the witness that checks it.
    pub fn tool_result(&self, key: &str, bytes: u32) -> Result<Value> {
        let (operation, child) = OperationId::parse(key)?;
        ensure!(
            operation.run == self.run(),
            "tool key `{key}` belongs to another run"
        );
        let Some(position) = child.strip_prefix("tool/") else {
            anyhow::bail!("`{key}` is not a tool call key");
        };
        self.record(
            operation.actor,
            operation.ordinal,
            &format!("tool-result/{position}"),
            bytes,
        )
    }
}

pub(super) fn tool_argument_padding(call: &ToolCallPlan) -> Result<usize> {
    let overhead = serde_json::to_vec(&json!({
        "record": {
            "kind": "synthetic",
            "key": call.idempotency_key,
            "result_bytes": call.result_bytes,
            "callback_ms": call.callback_ms,
        },
        "payload": "",
    }))?
    .len();
    ensure!(
        call.argument_bytes as usize >= overhead,
        "tool argument target {} cannot hold the {overhead}-byte call record of `{}`",
        call.argument_bytes,
        call.idempotency_key
    );
    Ok(call.argument_bytes as usize - overhead)
}

/// Arguments of the synthetic tool: the call's key, its result size and
/// callback delay, and vocabulary padding.
pub fn tool_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["record","payload"],"properties":{
        "record":{"type":"object","additionalProperties":false,"required":["kind","key","result_bytes","callback_ms"],"properties":{
            "kind":{"const":"synthetic"},
            "key":{"type":"string"},
            "result_bytes":{"type":"integer","minimum":0},
            "callback_ms":{"type":"integer","minimum":0}
        }},
        "payload":{"type":"string"}
    }})
}

/// A synthetic tool's result: a nested record padded to the sampled size.
pub fn tool_result_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["record","payload"],"properties":{
        "record":{"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"synthetic"}}},
        "payload":{"type":"string"}
    }})
}

/// Arguments of the witness mark a durable body makes once it has run.
pub fn mark_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["key"],"properties":{
        "key":{"type":"string"}
    }})
}

/// Arguments of the attachment put a cell makes for blob `index` of its turn.
pub fn attach_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["operation","index"],"properties":{
        "operation":{"type":"string"},
        "index":{"type":"integer","minimum":0}
    }})
}

fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = output.len();
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let crc = crc32fast::hash(&output[start..]);
    output.extend_from_slice(&crc.to_be_bytes());
}
