use super::{Case, Meter, Receipt};
use anyhow::{Result, ensure};
use lash_core::llm::types::ProviderRouteIdentity;
use lash_core::provider::{
    ProviderToken, TokenError, TokenRequest, TokenRequestReason, TokenSource,
};
use lash_llm_transport::TokenGate;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Barrier;

#[derive(Debug)]
struct Source {
    expired: bool,
    barrier: Barrier,
    meter: Meter,
}
#[async_trait::async_trait]
impl TokenSource for Source {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        let start = Instant::now();
        let token = if request.reason == TokenRequestReason::Current {
            // All callers acquire the same epoch before any replacement can
            // answer. No scheduler-dependent guess about concurrency.
            self.barrier.wait().await;
            if self.expired {
                ProviderToken::new("synthetic-old").expiring_at(SystemTime::UNIX_EPOCH)
            } else {
                ProviderToken::new("synthetic-old")
            }
        } else {
            tokio::task::yield_now().await;
            ProviderToken::new("synthetic-new")
        };
        self.meter.operation(
            match request.reason {
                TokenRequestReason::Current => "token.source.current",
                TokenRequestReason::Expiring => "token.source.expiring",
                TokenRequestReason::Rejected => "token.source.rejected",
                _ => "token.source.other",
            },
            format!("reason:{:?}", request.reason),
            "ok",
            start,
        );
        Ok(token)
    }
}

pub(super) async fn run(
    case: Case,
    waves: usize,
    callers: usize,
    ledger_cap: usize,
) -> Result<Receipt> {
    let meter = Meter::new(ledger_cap);
    for _ in 0..waves {
        let source = Arc::new(Source {
            expired: matches!(case, Case::TokenExpiring),
            barrier: Barrier::new(callers),
            meter: meter.clone(),
        });
        let gate = Arc::new(TokenGate::new(source, "boundary-token"));
        let route = ProviderRouteIdentity::for_endpoint(
            "boundary-token",
            "https://synthetic.invalid",
            "model",
        );
        let acquired = futures_util::future::try_join_all((0..callers).map(|_| async {
            let start = Instant::now();
            let lease = gate.current(&route).await?;
            meter.operation(
                "token.gate.current",
                format!("epoch:{}", lease.epoch),
                "ok",
                start,
            );
            Ok::<_, anyhow::Error>(lease)
        }))
        .await?;
        if matches!(case, Case::TokenRejected) {
            let leases = futures_util::future::try_join_all(acquired.iter().map(|old| async {
                let start = Instant::now();
                let lease = gate
                    .replace(&route, old, TokenRequestReason::Rejected)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("replacement absent"))?;
                meter.operation(
                    "token.gate.rejected",
                    format!("epoch:{}", lease.epoch),
                    "ok",
                    start,
                );
                ensure!(lease.epoch > old.epoch, "rejection did not renew the epoch");
                Ok::<_, anyhow::Error>(lease)
            }))
            .await?;
            ensure!(
                leases.iter().all(|lease| lease.epoch == 2),
                "replacement was not single flight"
            );
        } else {
            let expected = if matches!(case, Case::TokenExpiring) {
                2
            } else {
                1
            };
            ensure!(
                acquired.iter().all(|lease| lease.epoch == expected),
                "wrong token epoch"
            );
        }
    }
    ensure!(
        meter.count("token.source.current") == waves * callers,
        "missing current asks"
    );
    let refresh = match case {
        Case::TokenExpiring => "token.source.expiring",
        Case::TokenRejected => "token.source.rejected",
        _ => "token.source.none",
    };
    let expected = if matches!(case, Case::TokenHealthy) {
        0
    } else {
        waves
    };
    ensure!(
        meter.count(refresh) == expected,
        "replacement source was not single flight"
    );
    Ok(Receipt::new(
        case,
        "transport-token-gate",
        "synthetic-host-source",
        waves * callers,
        &meter,
        serde_json::json!({"waves": waves, "callers": callers, "replacements": expected,
            "expiry_skew_ms": Duration::from_secs(30).as_millis()}),
    ))
}
