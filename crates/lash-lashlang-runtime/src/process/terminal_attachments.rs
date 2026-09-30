use super::LashlangProcessHost;

/// A lashlang value carries a tool's attachment in the tagged shape, and the
/// terminal first holds it as untrusted JSON. The terminal delivers typed
/// only the attachments this process's record holds (ADR 0124 §4). Those
/// are its own puts, its deliveries and its start inputs. Every other
/// claim stays untrusted, so a guest cannot name an attachment into a
/// receiver it never held.
pub(super) async fn adopt_held_attachments(
    host: &LashlangProcessHost<'_>,
    output: &mut lash_core::ProcessAwaitOutput,
) -> Result<(), lash_core::ProcessInfraError> {
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        return Ok(());
    };
    let lash_core::ToolCallOutcome::Success(value) = &mut output.outcome else {
        return Ok(());
    };
    let claims = value.untrusted_attachment_claims();
    if claims.is_empty() {
        return Ok(());
    }
    let held = host
        .ctx
        .attachment_store()
        .claims_held_by_process_record(claims)
        .await
        .map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::Session(format!(
                "reading which attachments process `{}` holds for its terminal failed: {error}",
                host.process_id
            )))
        })?;
    *value = std::mem::replace(value, lash_core::ToolValue::Null).adopt_attachments(&held);
    Ok(())
}
