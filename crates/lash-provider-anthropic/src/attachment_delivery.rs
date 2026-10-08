use crate::support::*;
pub(crate) const CODEC: &str = "anthropic.messages.source";
impl AnthropicProvider {
    pub(crate) fn file_scope(&self) -> Option<ProviderFileScope> {
        self.attachment_credential_scope
            .as_ref()
            .map(|scope| ProviderFileScope {
                provider: self.kind().into(),
                endpoint: self.route_identity("").endpoint,
                credential_scope: scope.clone(),
            })
    }
    pub(crate) fn accepts_attachment(&self, mime: &lash_sansio::MediaType) -> ProviderAccepts {
        if !matches!(
            mime.as_str(),
            "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "application/pdf"
        ) {
            return ProviderAccepts::NONE;
        }
        ProviderAccepts {
            bytes: true,
            url: true,
            provider_file: self.file_scope(),
        }
    }
    pub(crate) fn encode_attachment(
        &self,
        slot: &AttachmentSlot,
        delivery: &Delivery,
    ) -> Result<TransientJson, LlmTransportError> {
        check_slot(slot, delivery, CODEC)?;
        if !slot
            .accepts
            .narrowed_to_live_scope(self.file_scope().as_ref())
            .allows(delivery)
        {
            return Err(template_error(
                "provider-file scope differs from the live scope",
            ));
        }
        let value = match delivery {
            Delivery::Bytes(bytes) => {
                json!({"type": "base64", "media_type": slot.reference.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes)})
            }
            Delivery::Url { url, .. } => json!({"type": "url", "url": url.expose()}),
            Delivery::ProviderFile { id, .. } => json!({"type": "file", "file_id": id.expose()}),
        };
        Ok(TransientJson::new(&value, delivery))
    }
}
