// FIG-4374: a session's recorded model is minted by the host's registry when
// a key is selected, and recorded; its fields are private, so host code
// cannot assemble one by literal.

fn a_recorded_model_is_minted_by_the_registry_only(metadata: lash::LlmProfileMetadata) {
    let _ = lash::RecordedLlmProfile {
        key: lash::LlmProfileKey::new("forged"),
        metadata,
    };
}

fn main() {}
