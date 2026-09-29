//! The controller-context methods that call a scope's `LashDurableWaitIndex`
//! object or a wait's `LashDurableWaitWorkflow` directly: peeks, resolves and
//! the session-wide revocation reads and updates.

/// Every controller context's calls on the durable-wait index and workflow.
macro_rules! durable_wait_index_methods {
    ($ctx:lifetime) => {
        fn peek_event<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            address: RestateDurableWaitAddress,
            replay_key: String,
        ) -> crate::JournaledFuture<'run, Option<Resolution>>
        where
            $ctx: 'run,
        {
            let request = namespace
                .durable_wait_workflow(self, address.workflow_key)
                .peek()
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
            Box::pin(async move {
                let resolution = request.call().await?.into_body();
                Ok(resolution)
            })
        }

        fn peek_turn_gate<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            key: lash_core::AwaitEventKey,
        ) -> crate::JournaledFuture<'run, RestateTurnGatePeek>
        where
            $ctx: 'run,
        {
            let address = RestateDurableWaitAddress::for_key(&key);
            let replay_key = key.key_id.clone();
            let request = namespace
                .durable_wait_registry(self, durable_wait_index_object_key(&address))
                .peek_turn_gate(RestateDurableWaitIndexRequest { key })
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
            let call = request.call();
            Box::pin(async move {
                let peek = call.await?.into_body();
                Ok(peek)
            })
        }

        fn resolve_event<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitResolveRequest,
        ) -> ResolveEventFuture<'run>
        where
            $ctx: 'run,
        {
            Box::pin(async move {
                let replay_key = request.key.key_id.clone();
                let address = RestateDurableWaitAddress::for_key(&request.key);
                let resolve = namespace
                    .durable_wait_registry(self, durable_wait_index_object_key(&address))
                    .resolve(request)
                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                let outcome = resolve.call().await?.into_body();
                Ok(outcome)
            })
        }

        fn publish_event<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitResolveRequest,
        ) -> crate::JournaledFuture<'run, ()>
        where
            $ctx: 'run,
        {
            let replay_key = request.key.key_id.clone();
            let address = RestateDurableWaitAddress::for_key(&request.key);
            let send = namespace
                .durable_wait_registry(self, durable_wait_index_object_key(&address))
                .resolve(request)
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                .send();
            Box::pin(async move {
                send.await?;
                Ok(())
            })
        }

        fn update_session_waits<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            session_id: SessionId,
            revoke: bool,
        ) -> crate::JournaledFuture<'run, ()>
        where
            $ctx: 'run,
        {
            let client = namespace.durable_wait_registry(self, session_id);
            let request = if revoke {
                client.revoke_all()
            } else {
                client.cancel_all()
            };
            let call = request.call();
            Box::pin(async move {
                call.await?;
                Ok(())
            })
        }

        fn session_is_revoked<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            session_id: SessionId,
        ) -> crate::JournaledFuture<'run, bool>
        where
            $ctx: 'run,
        {
            let request = namespace
                .durable_wait_registry(self, session_id)
                .is_revoked();
            let call = request.call();
            Box::pin(async move {
                let revoked = call.await?.into_body();
                Ok(revoked)
            })
        }
    };
}
