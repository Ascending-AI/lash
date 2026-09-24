//! The config-settlement laws' only leg. They drive a runtime on the
//! in-process effect host, and move to the Restate test engine with the other
//! engine laws (FIG-3665).

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn session_config_settlement_timeout_is_typed() {
        Box::pin(crate::session_config_settlement_timeout_is_typed()).await;
    }

    #[tokio::test]
    async fn cancelled_session_config_settlement_is_typed() {
        crate::cancelled_session_config_settlement_is_typed().await;
    }

    #[tokio::test]
    async fn superseded_config_settlement_adopts_the_newer_head() {
        Box::pin(crate::superseded_config_settlement_adopts_the_newer_head()).await;
    }
}
