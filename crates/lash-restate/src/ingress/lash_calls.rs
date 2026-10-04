//! Ingress calls to lash's own handlers, which take a [`Call`] and answer a
//! [`Reply`] (ADR 0115 §3.1).

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{RestateHttpError, RestateIngressClient, RestateInvocationId};
use crate::compat::{Call, Reply};

#[allow(
    clippy::result_large_err,
    reason = "the lash calls answer the ingress client's own RestateHttpError"
)]
impl RestateIngressClient {
    /// A call to a handler of one of lash's own objects: the body travels in
    /// a [`Call`] at this build's wire range and the answer in a [`Reply`]
    /// (ADR 0115 §3.1). A handler without a request takes `&()`.
    pub(crate) async fn call_lash_object<T, R>(
        &self,
        object: &str,
        object_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_object_json::<_, Reply<R>>(object, object_key, handler, &Call::new(body))
            .await
            .map(Reply::into_body)
    }

    /// [`call_lash_object`](Self::call_lash_object) for one of lash's own
    /// workflows.
    pub(crate) async fn call_lash_workflow<T, R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_workflow_json::<_, Reply<R>>(workflow, workflow_key, handler, &Call::new(body))
            .await
            .map(Reply::into_body)
    }

    /// [`call_lash_workflow`](Self::call_lash_workflow) under an idempotency
    /// key.
    pub(crate) async fn call_lash_workflow_idempotent<T, R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
        idempotency_key: &str,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_workflow_json_idempotent::<_, Reply<R>>(
            workflow,
            workflow_key,
            handler,
            &Call::new(body),
            idempotency_key,
        )
        .await
        .map(Reply::into_body)
    }

    /// A send to a handler of one of lash's own workflows, its body in a
    /// [`Call`] at this build's wire range.
    pub(crate) async fn send_lash_workflow<T: Serialize + ?Sized>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        self.send_workflow_json(workflow, workflow_key, handler, &Call::new(body))
            .await
    }

    /// [`send_lash_workflow`](Self::send_lash_workflow) under an idempotency
    /// key.
    pub(crate) async fn send_lash_workflow_idempotent<T: Serialize + ?Sized>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
        idempotency_key: &str,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        self.send_workflow_json_idempotent(
            workflow,
            workflow_key,
            handler,
            &Call::new(body),
            idempotency_key,
        )
        .await
    }
}
