impl AppState {
    /// Observe a turn terminal through both the process-local attachment and
    /// its durable Restate address, allowing cancellation after replacement.
    async fn await_turn_terminal(
        &self,
        live_driver: &lash::TurnWorkDriver,
        address: &lash::TurnAddress,
    ) -> Result<lash::TurnTerminal, lash::runtime::RuntimeError> {
        let durable_driver =
            lash_restate::RestateTurnDeployment::new(lash_restate::RestateConnection::with_client(
                self.restate_ingress_url.clone(),
                self.restate_http.clone(),
            ))
            .turn_work_driver();
        let live = live_driver.await_terminal(address);
        let durable = durable_driver.await_terminal(address);
        tokio::pin!(live);
        tokio::pin!(durable);

        tokio::time::timeout(TURN_TERMINAL_ATTACH_TIMEOUT, async {
            tokio::select! {
                result = &mut live => match result {
                    Ok(terminal) => Ok(terminal),
                    Err(_) => durable.await,
                },
                result = &mut durable => match result {
                    Ok(terminal) => Ok(terminal),
                    Err(_) => live.await,
                },
            }
        })
        .await
        .map_err(|_| {
            lash::runtime::RuntimeError::new(
                lash::runtime::RuntimeErrorCode::TurnTerminalAwaitTimeout,
                format!(
                    "timed out awaiting terminal for turn `{}` in session `{}` after {} ms",
                    address.turn_id,
                    address.session_id,
                    TURN_TERMINAL_ATTACH_TIMEOUT.as_millis()
                ),
            )
        })?
    }
}
