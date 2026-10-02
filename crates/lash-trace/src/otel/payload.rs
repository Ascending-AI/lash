use super::{OtelOptions, OtelPayloadExport, registry::AttributeKey as A};
use crate::TraceRecord;
use opentelemetry::KeyValue;
use std::io::{self, Write};

pub(super) struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    pub truncated: bool,
}
impl BoundedWriter {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn finish(self) -> String {
        let mut bytes = self.bytes;
        while std::str::from_utf8(&bytes).is_err() {
            bytes.pop();
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
impl Write for BoundedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        let written = buf.len().min(remaining);
        self.bytes.extend_from_slice(&buf[..written]);
        if written < buf.len() {
            self.truncated = true;
            return Err(io::Error::other("telemetry byte limit"));
        }
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn attributes(record: &TraceRecord, options: &OtelOptions, out: &mut Vec<KeyValue>) {
    let (mut remaining, events) = match options.payloads {
        OtelPayloadExport::Off => (4096, 0),
        OtelPayloadExport::Bounded {
            max_record_bytes,
            max_events,
        } => (max_record_bytes, max_events),
    };
    let mut truncated = 0_i64;
    if options.include_context_metadata {
        let mut writer = BoundedWriter::new(remaining);
        let _ = serde_json::to_writer(&mut writer, &record.context.metadata);
        remaining = remaining.saturating_sub(writer.len());
        truncated += i64::from(writer.truncated);
        out.push(A::ContextMetadata.value(writer.finish()));
    }
    if let OtelPayloadExport::Bounded { .. } = options.payloads {
        if events == 0 {
            out.push(A::EventsOmitted.value(1_i64));
        } else {
            let mut writer = BoundedWriter::new(remaining);
            let _ = serde_json::to_writer(&mut writer, &record.event);
            truncated += i64::from(writer.truncated);
            out.push(A::Payload.value(writer.finish()));
        }
    }
    if truncated != 0 {
        out.push(A::PayloadTruncated.value(true));
        out.push(A::PayloadsTruncated.value(truncated));
    }
}
