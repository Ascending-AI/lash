use std::time::Duration;

use crate::RestateConnection;

impl super::LiveConformanceHarness {
    pub(in crate::tests) async fn finish_group_law(&self, law: &str) {
        let reason = match law {
            "a_close_may_narrow_but_never_widen" => {
                Some("the widening-refusal probe deliberately leaves its never-ending child open")
            }
            "a_reopen_dispatches_the_retained_membership" => {
                Some("the membership-reopen probe deliberately parks both retained children")
            }
            "the_first_settlement_wakes_the_caller_while_the_loser_still_runs" => Some(
                "the first-settlement law deliberately leaves its losing child behind a closed gate",
            ),
            _ => None,
        };
        if let Some(reason) = reason {
            self.kill_open(reason).await;
        }
        self.finish().await;
    }
}

/// The harness's admin face: where it modifies retained service state and
/// controls invocations.
#[derive(Clone)]
pub(in crate::tests) enum HarnessAdmin {
    Live {
        admin_url: String,
    },
    InProcess {
        server: lash_restate_test::RestateTestServer,
    },
}

/// Every invocation of `server` that has not completed — its status,
/// attempts, last failure and the journal it holds — and the timers pending
/// on it: what a law that waited an invocation out in vain reports.
pub(super) fn open_invocations_report(server: &lash_restate_test::RestateTestServer) -> String {
    let mut report = String::new();
    for view in server.invocations() {
        if view.status == "completed" {
            continue;
        }
        let journal: Vec<_> = server
            .journal(&view.id)
            .unwrap_or_default()
            .into_iter()
            .map(|entry| format!("{:?}:{:?}", entry.ty, entry.name))
            .collect();
        report.push_str(&format!("\n  {view:?} journal={journal:?}"));
    }
    report.push_str(&format!("\n  timers {:?}", server.timers()));
    report
}

impl HarnessAdmin {
    /// The engine's open invocations and pending timers, for a law's failure
    /// message. A live server's are read from its own admin API instead.
    pub(in crate::tests) fn open_invocations_report(&self) -> String {
        match self {
            Self::InProcess { server } => open_invocations_report(server),
            Self::Live { admin_url } => {
                format!("\n  (a live server: read its invocations at {admin_url})")
            }
        }
    }

    pub(super) fn connection(&self) -> RestateConnection {
        match self {
            Self::Live { admin_url } => RestateConnection::new(admin_url.clone()),
            Self::InProcess { server } => {
                RestateConnection::with_transport(server.ingress_url(), server.transport())
            }
        }
    }

    /// Whether the invocation of workflow `service`'s `run` handler under
    /// `key` is paused: its retry policy spent its attempts, and it runs
    /// nothing more until an operator resumes it.
    pub(in crate::tests) async fn workflow_paused(&self, service: &str, key: &str) -> bool {
        match self {
            Self::InProcess { server } => server
                .find_invocation(service, key, "run", "paused")
                .is_some(),
            Self::Live { admin_url } => {
                crate::RestateAdminClient::new(RestateConnection::new(admin_url.clone()))
                    .workflow_invocation_status(service, key, "run")
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|status| status.status.as_str() == "paused")
            }
        }
    }

    /// Kills the open invocation of workflow `service`'s `run` handler under
    /// `key`, as an operator does, and returns its id once the killed
    /// execution can run no more work: an abort only stops an attempt's task
    /// at its next yield, so on the in-process server this waits for the
    /// killed attempt's task to end — a caller that serves a resubmission
    /// next must not be overtaken by the killed poll still in flight. A
    /// live server cannot report its deployment-side tasks, so there the
    /// kill's own acknowledgement is all there is.
    pub(in crate::tests) async fn kill_workflow_run(&self, service: &str, key: &str) -> String {
        match self {
            Self::InProcess { server } => {
                let target = format!("{service}/{key}/run");
                let open = server
                    .invocations()
                    .into_iter()
                    .find(|view| view.target == target && view.status != "completed")
                    .unwrap_or_else(|| panic!("an open invocation of `{target}`"));
                assert_eq!(
                    server.kill_and_await(&open.id).await,
                    Some(true),
                    "kill the open invocation of `{target}`"
                );
                open.id
            }
            Self::Live { admin_url } => {
                let admin =
                    crate::RestateAdminClient::new(RestateConnection::new(admin_url.clone()));
                let open = admin
                    .workflow_invocation_status(service, key, "run")
                    .await
                    .unwrap_or_else(|error| panic!("find the run of `{service}/{key}`: {error}"))
                    .unwrap_or_else(|| panic!("an invocation of `{service}/{key}/run`"));
                admin
                    .kill_invocation(&crate::RestateInvocationId::new(open.id.clone()))
                    .await
                    .unwrap_or_else(|error| panic!("kill `{}`: {error}", open.id));
                open.id
            }
        }
    }

    /// Purges the completed invocation `id` as the retention sweep does: its
    /// journal is gone, and its workflow key starts over.
    pub(in crate::tests) async fn purge_invocation(&self, id: &str) {
        match self {
            Self::InProcess { server } => {
                // A kill completes the invocation on its own schedule: purge
                // once it has.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                loop {
                    match server.purge(id) {
                        Some(true) => return,
                        Some(false) if tokio::time::Instant::now() < deadline => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        other => panic!("purge `{id}`: {other:?}"),
                    }
                }
            }
            Self::Live { admin_url } => {
                let client = reqwest::Client::builder()
                    .http2_prior_knowledge()
                    .build()
                    .expect("build Restate admin client");
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                loop {
                    let response = client
                        .patch(format!(
                            "{}/invocations/{id}/purge",
                            admin_url.trim_end_matches('/')
                        ))
                        .send()
                        .await
                        .unwrap_or_else(|error| panic!("purge `{id}`: {error}"));
                    let status = response.status();
                    if status.is_success() {
                        return;
                    }
                    let body = response.text().await.unwrap_or_default();
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "purge `{id}`: {status} {body}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}
