//! L20: recovery reads and resumes the invocation that owns the Run.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lash_http_transport::{
    HttpRequest, HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError,
};

#[derive(Debug)]
pub(super) struct NoGroupCatalog {
    inner: Option<Arc<dyn HttpTransport>>,
}

impl NoGroupCatalog {
    pub(super) fn connection(
        server: &lash_restate_test::RestateTestServer,
    ) -> crate::RestateConnection {
        crate::RestateConnection::with_transport(
            server.ingress_url(),
            Arc::new(Self {
                inner: Some(server.transport()),
            }),
        )
    }
}

#[async_trait::async_trait]
impl HttpTransport for NoGroupCatalog {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            !body.contains("EffectGroup")
                && !request.url.contains("EffectGroup")
                && !request.url.contains("retired-child"),
            "L20: recovery requested the unavailable group catalog: {} {body}",
            request.url,
        );
        if let Some(inner) = &self.inner {
            return inner.send(request, timeout).await;
        }
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: HttpResponseBody::buffered(r#"{"rows":[]}"#),
        })
    }
}

#[tokio::test]
async fn l20_recovery_queries_only_owner_invocations() {
    let admin = crate::RestateAdminClient::new(crate::RestateConnection::with_transport(
        "https://restate.example",
        Arc::new(NoGroupCatalog { inner: None }),
    ));
    for namespace in [
        crate::RestateNamespace::default(),
        crate::RestateNamespace::new("owner-park").unwrap(),
    ] {
        assert!(
            admin
                .paused_work_page(&namespace, None, NonZeroUsize::MIN)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
