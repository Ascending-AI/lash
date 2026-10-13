fn main() {
    let _metadata = lash::LlmProfileMetadata::builder("provider/model")
        .context_window_tokens(8_192)
        .build();
}
