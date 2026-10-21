//! Mapping process reader outcomes into the same guest errors as session reads.

use lash_execution_contracts::process::EngineReadError;
use lash_kernel_doc::{Datum, ErrorDatum};
use lash_vm_broker::kernel::{EFFECT_CANCELLED, EFFECT_RESULT, EFFECT_TIMED_OUT, datum_from_json};

pub(super) fn error(kind: &str, message: String, data: serde_json::Value) -> ErrorDatum {
    ErrorDatum {
        kind: kind.to_owned(),
        message,
        data: datum_from_json(&data.to_string()).unwrap_or(Datum::Null),
    }
}

pub(super) fn answer(
    answer: Result<serde_json::Value, EngineReadError>,
) -> Result<Datum, ErrorDatum> {
    match answer {
        Ok(value) => datum_from_json(&value.to_string())
            .map_err(|invalid| error(EFFECT_RESULT, invalid.to_string(), serde_json::Value::Null)),
        Err(EngineReadError::Arguments(message)) => Err(error(
            super::EFFECT_ARGUMENTS,
            message,
            serde_json::Value::Null,
        )),
        Err(EngineReadError::Tool(failure)) => Err(error(
            super::TOOL_FAILED,
            failure.message.clone(),
            failure.to_json_value(),
        )),
        Err(EngineReadError::Reader { message, data }) => {
            Err(error(super::TOOL_FAILED, message, data))
        }
        Err(EngineReadError::Cancelled) => Err(error(
            EFFECT_CANCELLED,
            "the read was stopped with the process".to_owned(),
            serde_json::Value::Null,
        )),
        Err(EngineReadError::TimedOut) => Err(error(
            EFFECT_TIMED_OUT,
            "the read exceeded its tool or parent deadline".to_owned(),
            serde_json::Value::Null,
        )),
    }
}
