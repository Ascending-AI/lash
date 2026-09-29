use super::{Generator, TurnPlan};
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

    pub fn tool_argument(&self, actor: u64, ordinal: u64, bytes: u32) -> Result<Value> {
        let mut record = json!({"record":{"kind":"synthetic"},"payload":""});
        let overhead = serde_json::to_vec(&record)?.len();
        ensure!(
            bytes as usize >= overhead,
            "tool argument target is too small"
        );
        let padding = bytes as usize - overhead;
        let word = self.tool_word(actor, ordinal);
        record["payload"] = format!(
            "{}{}",
            word.repeat(padding / word.len()),
            &word[..padding % word.len()]
        )
        .into();
        Ok(record)
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
        for (batch, calls) in plan.tool_batches.iter().enumerate() {
            for (index, call) in calls.iter().enumerate() {
                tool_arguments.push(self.tool_argument(
                    id.actor,
                    id.ordinal,
                    call.argument_bytes,
                )?);
                tool_results.push(self.record(
                    id.actor,
                    id.ordinal,
                    &format!("tool-result/{batch}/{index}"),
                    call.result_bytes,
                )?);
            }
        }
        let mut attachments = Vec::new();
        for (index, attachment) in plan.attachments.iter().enumerate() {
            let actor = attachment.owner_actors.first().copied().unwrap_or(id.actor);
            let bytes = self.png(
                actor,
                id.ordinal,
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
            attachments.push(generated);
        }
        Ok(SyntheticPayloads {
            prompt: self.text(id.actor, id.ordinal, "prompt", plan.prompt_bytes),
            input: self.record(id.actor, id.ordinal, "input", plan.input_bytes)?,
            tool_schema: tool_schema(),
            tool_arguments,
            tool_results,
            attachments,
        })
    }
}

pub fn tool_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["record","payload"],"properties":{
        "record":{"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"synthetic"}}},
        "payload":{"type":"string"}
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
