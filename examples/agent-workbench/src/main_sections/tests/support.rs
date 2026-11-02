/// A committed message's parts' content, one per line: how the laws read
/// what a message says.
pub(crate) fn message_text(message: &lash::messages::Message) -> String {
    message
        .parts
        .iter()
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A committed message's role, by name.
pub(crate) fn message_role(message: &lash::messages::Message) -> &'static str {
    match message.role {
        lash::messages::MessageRole::User => "user",
        lash::messages::MessageRole::Assistant => "assistant",
        lash::messages::MessageRole::System => "system",
        lash::messages::MessageRole::Event => "event",
    }
}
