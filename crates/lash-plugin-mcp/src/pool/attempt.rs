use super::*;

impl McpConnectionPool {
    pub(super) async fn call_resolved_tool(
        &self,
        target: ResolvedToolTarget,
        args: &Value,
        context: &AttemptContext<'_>,
    ) -> ToolOutcome {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let ResolvedToolTarget {
            entry,
            advertised_name,
            native_name,
            binding,
        } = target;

        let call_timeout = entry.config.call_timeout();
        let server_name = entry.server_name.clone();
        let arguments = match args {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => {
                return McpCallFailure::InvalidArguments {
                    tool: advertised_name,
                    arguments: other.clone(),
                }
                .into();
            }
        };

        // The actor publishes a peer/generation snapshot through `watch`.
        // Dispatch clones that cheap handle without awaiting or routing calls
        // through lifecycle coordination.
        let (peer, service_generation) = {
            match entry.service_snapshot() {
                Some(service) => (service.peer.clone(), service.generation),
                None => {
                    return McpCallFailure::ServerUnavailable {
                        server: server_name,
                        health: entry.health.read_recover().clone(),
                        after_ms: entry.config.reconnect_initial_backoff().as_millis() as u64,
                    }
                    .into();
                }
            }
        };

        // `RunningService::is_closed()` only reflects explicit cancellation or
        // whether its join handle was taken; a dead child transport can still
        // report open. Health checks must use the peer transport sender.
        if peer.is_transport_closed() {
            let cause =
                format!("MCP server `{server_name}` transport was closed before tool dispatch");
            entry.mark_disconnected(cause.clone(), service_generation);
            return McpCallFailure::ConnectionLost {
                server: server_name,
                cause: McpServiceFailure::TransportClosed,
                after_ms: entry.config.reconnect_initial_backoff().as_millis() as u64,
                shutting_down: entry.is_shutting_down(),
            }
            .into();
        }

        if let Some(binding) = &binding {
            if binding.completion != RemoteCompletion::Inline {
                return McpCallFailure::UnsupportedRemoteCompletion {
                    server: binding.server.clone(),
                    tool_id: naming::durable_tool_id(&binding.server, &binding.native_tool)
                        .to_string(),
                }
                .into();
            }
            if admission::peer_digest(&peer).ok().as_ref() != Some(&binding.peer_digest) {
                return McpCallFailure::ExecutionBindingChanged {
                    server: server_name,
                    tool_id: naming::durable_tool_id(&binding.server, &binding.native_tool)
                        .to_string(),
                }
                .into();
            }
        }
        if context
            .cancellation_token()
            .is_some_and(|token| token.is_cancelled())
        {
            return ToolOutcome::cancelled("MCP call cancelled before send");
        }
        let mut params = CallToolRequestParams::new(native_name);
        params.arguments = arguments;
        params.meta = Some(rmcp::model::Meta(serde_json::Map::from_iter([
            (
                "lash.dev/tool-call-id".into(),
                Value::String(context.call_id().to_string()),
            ),
            (
                "lash.dev/tool-attempt".into(),
                Value::from(context.attempt_number()),
            ),
        ])));
        let mut options = PeerRequestOptions::with_timeout(call_timeout)
            .with_max_total_timeout(entry.config.call_max_total_timeout());
        if entry.config.reset_call_timeout_on_progress() {
            options = options.reset_timeout_on_progress();
        }
        let response = match peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(params)),
                options,
            )
            .await
        {
            Ok(handle) => {
                let request_id = handle.id.clone();
                let response = handle.await_response();
                tokio::pin!(response);
                if let Some(token) = context.cancellation_token() {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            let reason = "Lash attempt cancelled".to_string();
                            let _ = peer.send_notification(rmcp::model::CancelledNotification::new(
                                rmcp::model::CancelledNotificationParam { request_id, reason: Some(reason.clone()) },
                            ).into()).await;
                            // Let rmcp finish request cleanup and remove its progress
                            // watcher. The notification gives no remote termination receipt.
                            let _ = response.await;
                            Err(ServiceError::Cancelled { reason: Some(reason) })
                        }
                        result = &mut response => result,
                    }
                } else {
                    response.await
                }
            }
            Err(err) => Err(err),
        };

        match response {
            Ok(ServerResult::CallToolResult(result)) => {
                entry.record_call_success(service_generation);
                tool_result_from_rmcp(result, context).await
            }
            Ok(_) => McpCallFailure::UnexpectedResponse.into(),
            Err(ServiceError::Timeout { timeout }) => {
                entry
                    .handle_call_timeout(&peer, service_generation, timeout)
                    .await
            }
            Err(ServiceError::Cancelled { reason }) => ToolOutcome::cancelled(format!(
                "MCP tool call on `{server_name}` was cancelled{}",
                reason
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            )),
            Err(err) => match McpServiceFailure::from(err) {
                cause @ (McpServiceFailure::TransportClosed
                | McpServiceFailure::TransportSend { .. }) => {
                    entry.mark_disconnected(
                        format!("MCP server `{server_name}` connection lost: {cause:?}"),
                        service_generation,
                    );
                    McpCallFailure::ConnectionLost {
                        server: server_name,
                        cause,
                        after_ms: entry.config.reconnect_initial_backoff().as_millis() as u64,
                        shutting_down: entry.is_shutting_down(),
                    }
                    .into()
                }
                McpServiceFailure::JsonRpc { error } => McpCallFailure::JsonRpc { error }.into(),
                McpServiceFailure::UnexpectedResponse => McpCallFailure::UnexpectedResponse.into(),
                McpServiceFailure::UnsupportedSdkError { diagnostic } => {
                    McpCallFailure::UnsupportedSdkError { diagnostic }.into()
                }
                McpServiceFailure::Timeout { .. }
                | McpServiceFailure::Cancelled { .. }
                | McpServiceFailure::ConsecutiveTimeouts { .. } => unreachable!(
                    "timeout and cancellation handled above; consecutive timeout is actor-only"
                ),
            },
        }
    }
}
