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
        let (peer, service_generation, handler) = {
            match entry.service_snapshot() {
                Some(service) => (
                    service.peer.clone(),
                    service.generation,
                    service.handler.clone(),
                ),
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
                reconnect_attempts: entry.config.reconnect_max_attempts(),
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
        params.meta = Some(rmcp::model::RequestMetaObject::from(
            serde_json::Map::from_iter([
                (
                    "lash.dev/tool-call-id".into(),
                    Value::String(context.call_id().to_string()),
                ),
                (
                    "lash.dev/tool-attempt".into(),
                    Value::from(context.attempt_number()),
                ),
            ]),
        ));
        let cancellation = context
            .cancellation_token()
            .map(tokio_util::sync::CancellationToken::child_token)
            .unwrap_or_default();
        let total = entry.config.call_max_total_timeout();
        let deadline = tokio::time::Instant::now() + total;
        let rounds = async {
            for _ in 0..rmcp::model::DEFAULT_MRTR_MAX_ROUNDS {
                let mut options = PeerRequestOptions::with_timeout(call_timeout)
                    .with_max_total_timeout(
                        deadline.saturating_duration_since(tokio::time::Instant::now()),
                    );
                if entry.config.reset_call_timeout_on_progress() {
                    options = options.reset_timeout_on_progress();
                }
                // Hold receive-side correlation across send: even an immediate
                // notification waits until the SDK token is bound to this call.
                let mut routes = handler.observations.lock().await;
                let sent = peer
                    .send_cancellable_request(
                        ClientRequest::CallToolRequest(Request::new(params.clone())),
                        options,
                    )
                    .await;
                let mut route_lease = if let Ok(handle) = &sent {
                    routes
                        .register(handle.progress_token.clone(), handle.id.clone(), context)
                        .then(|| {
                            crate::observations::RouteLease::new(
                                handler.observations.clone(),
                                handle.id.clone(),
                            )
                        })
                } else {
                    None
                };
                drop(routes);
                let response = match sent {
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
                                        rmcp::model::CancelledNotificationParam::new(Some(request_id), Some(reason.clone())),
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

                if let Some(lease) = &mut route_lease {
                    lease.finish().await;
                }
                match response? {
                    result @ ServerResult::CallToolResult(_) => return Ok(result),
                    result @ (ServerResult::InputRequiredResult(_)
                    | ServerResult::CustomResult(_)) => {
                        let retry = tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => return Err(ServiceError::Cancelled { reason: Some("Lash attempt cancelled".into()) }),
                            retry = tokio::time::timeout_at(deadline, super::round_trip::fulfill(result, &handler, &peer, &cancellation)) => retry.map_err(|_| ServiceError::Timeout { timeout: total })??,
                        };
                        params.input_responses = retry.responses;
                        params.request_state = retry.state;
                    }
                    _ => return Err(ServiceError::UnexpectedResponse),
                }
            }
            Err(ServiceError::McpError(
                rmcp::model::ErrorData::invalid_params(
                    "MCP multi round-trip limit exceeded",
                    Some(serde_json::json!({"max_rounds": rmcp::model::DEFAULT_MRTR_MAX_ROUNDS})),
                ),
            ))
        };
        let response = rounds.await.map_err(|error| match error {
            ServiceError::Timeout { timeout } if timeout != call_timeout => {
                ServiceError::Timeout { timeout: total }
            }
            error => error,
        });
        // MRTR host work observes cancellation on both local cancellation and
        // the total deadline, including when a host task has been spawned.
        cancellation.cancel();

        match response {
            Ok(ServerResult::CallToolResult(result)) => {
                entry.record_call_success(service_generation);
                tool_result_from_rmcp(result, context).await
            }
            Ok(_) => McpCallFailure::UnexpectedResponse.into(),
            Err(ServiceError::Timeout { timeout }) => {
                entry.handle_call_timeout(service_generation, timeout).await
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
                        reconnect_attempts: entry.config.reconnect_max_attempts(),
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
