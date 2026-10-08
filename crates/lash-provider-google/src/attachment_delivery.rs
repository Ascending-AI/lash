use crate::support::*;
pub(crate) const CODEC: &str = "google.gemini.part";
impl GoogleOAuthProvider {
    pub(crate) fn file_scope(&self) -> Option<ProviderFileScope> {
        let scope = self.attachment_credential_scope.as_ref()?;
        let project = self
            .project_id
            .as_deref()
            .or(self.resolved_project_id.get().map(String::as_str))?;
        Some(ProviderFileScope {
            provider: self.kind().into(),
            endpoint: self.route_identity_for_model("").endpoint.into(),
            credential_scope: format!("{}:{scope}{project}", scope.len()),
        })
    }
    pub(crate) fn accepts_attachment(&self, mime: &lash_sansio::MediaType) -> ProviderAccepts {
        if mime.as_str() != "application/pdf"
            && !matches!(mime.family(), "image" | "audio" | "text" | "video")
        {
            return ProviderAccepts::NONE;
        }
        ProviderAccepts {
            bytes: true,
            url: false,
            provider_file: self.file_scope(),
        }
    }
    pub(crate) fn encode_attachment(
        &self,
        slot: &AttachmentSlot,
        delivery: &Delivery,
    ) -> Result<TransientJson, LlmTransportError> {
        check_codec(slot, CODEC)?;
        let value = match delivery {
            Delivery::Bytes(bytes) => json!({"inlineData": {"mimeType": slot.reference.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes)}}),
            Delivery::ProviderFile { id, .. } => {
                json!({"fileData": {"mimeType": slot.reference.media_type, "fileUri": id.expose()}})
            }
            Delivery::Url { .. } => {
                return Err(template_error(
                    "Gemini does not encode ordinary URL deliveries",
                ));
            }
        };
        Ok(TransientJson::new(&value, delivery))
    }
}
