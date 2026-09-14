//! Input normalization and message-delta utilities used by the runtime.
//!
//! Extracted from `runtime/mod.rs`. These helpers are crate-internal.

use super::{InputItem, NormalizedItem};

pub async fn normalize_input_items(
    items: &[InputItem],
    attachment_store: &crate::SessionAttachmentStore,
    attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<Vec<NormalizedItem>, String> {
    let mut out: Vec<NormalizedItem> = Vec::new();
    for item in items {
        match item {
            InputItem::Text { text } => push_text(&mut out, text.clone()),
            InputItem::Attachment { source } => {
                attachment_source_policy
                    .authorize(&crate::AttachmentProducer::TurnIngress, source)
                    .map_err(|err| err.to_string())?;
                let source = match source {
                    crate::AttachmentSource::Inline { media_type, bytes } => {
                        let reference = attachment_store
                            .put(
                                bytes.clone(),
                                crate::AttachmentCreateMeta::new(media_type.clone(), None, None),
                            )
                            .await
                            .map_err(|err| format!("Failed to store inline attachment: {err}"))?;
                        crate::AttachmentSource::stored(reference)
                    }
                    borrowed_or_stored => borrowed_or_stored.clone(),
                };
                out.push(NormalizedItem::Attachment(source));
            }
        }
    }
    Ok(out)
}

fn push_text(out: &mut Vec<NormalizedItem>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(NormalizedItem::Text(last)) = out.last_mut() {
        last.push_str(&text);
    } else {
        out.push(NormalizedItem::Text(text));
    }
}
