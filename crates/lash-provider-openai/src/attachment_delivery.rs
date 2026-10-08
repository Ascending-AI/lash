use crate::support::*;
pub(crate) const CHAT_CODEC: &str = "openai.chat.image_url";
pub(crate) const RESPONSES_CODEC: &str = "openai.responses.input";
pub(crate) fn image(mime: &lash_sansio::MediaType) -> bool {
    matches!(
        mime.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}
pub(crate) fn responses_accepts(
    mime: &lash_sansio::MediaType,
    scope: Option<ProviderFileScope>,
) -> ProviderAccepts {
    let image = image(mime);
    let file = matches!(
        mime.as_str(),
        "application/pdf"
            | "application/json"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "text/csv"
            | "text/html"
            | "text/markdown"
            | "text/plain"
    );
    if !image && !file {
        return ProviderAccepts::NONE;
    }
    ProviderAccepts {
        bytes: true,
        url: true,
        provider_file: if file { scope } else { None },
    }
}
pub(crate) fn encode(
    slot: &AttachmentSlot,
    delivery: &Delivery,
    codec: &str,
) -> Result<TransientJson, LlmTransportError> {
    check_codec(slot, codec)?;
    let mime = &slot.reference.media_type;
    let value = if codec == CHAT_CODEC {
        match delivery {
            Delivery::Bytes(bytes) => Value::String(crate::request_work::attachment_data_url(
                mime.as_str(),
                bytes,
            )),
            Delivery::Url { url, .. } => Value::String(url.expose().into()),
            Delivery::ProviderFile { .. } => {
                return Err(template_error(
                    "Chat does not encode provider-file deliveries",
                ));
            }
        }
    } else if image(mime) {
        match delivery {
            Delivery::Bytes(bytes) => {
                json!({"type": "input_image", "image_url": crate::request_work::attachment_data_url(mime.as_str(), bytes)})
            }
            Delivery::Url { url, .. } => json!({"type": "input_image", "image_url": url.expose()}),
            Delivery::ProviderFile { .. } => {
                return Err(template_error("image slots do not encode file ids"));
            }
        }
    } else {
        match delivery {
            Delivery::Bytes(bytes) => {
                json!({"type": "input_file", "file_data": crate::request_work::attachment_data_url(mime.as_str(), bytes)})
            }
            Delivery::Url { url, .. } => json!({"type": "input_file", "file_url": url.expose()}),
            Delivery::ProviderFile { id, .. } => {
                json!({"type": "input_file", "file_id": id.expose()})
            }
        }
    };
    Ok(TransientJson::new(&value, delivery))
}
