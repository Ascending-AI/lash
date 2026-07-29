use super::*;

/// Process leases recover by TTL: an unexpired stale holder remains busy, and
/// the first claimant after expiry acquires a higher fencing token.
pub(super) async fn process_lease_ttl_contract(registry: Arc<dyn ProcessRegistry>) {
    let stale_holder = process_lease_owner("stale-holder");
    let claimant = process_lease_owner("ttl-claimant");
    registry
        .register_process(registration("proc-lease-ttl"))
        .await
        .expect("register process lease TTL case");
    let holder = registry
        .claim_process_lease("proc-lease-ttl", &stale_holder, 50)
        .await
        .expect("claim stale-holder lease")
        .acquired()
        .expect("stale-holder lease acquired");

    let busy = registry
        .reclaim_process_lease("proc-lease-ttl", &claimant, &holder, 60_000)
        .await
        .expect("retry process lease claim before TTL");
    assert!(
        matches!(
            busy,
            crate::ProcessLeaseClaimOutcome::Busy {
                holder: ref busy_holder
            }
                if busy_holder.lease_token == holder.lease_token
        ),
        "an unexpired stale process lease must remain busy"
    );

    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    let acquired = registry
        .reclaim_process_lease("proc-lease-ttl", &claimant, &holder, 60_000)
        .await
        .expect("retry process lease claim after TTL")
        .acquired()
        .expect("stale process lease must become claimable after TTL");
    assert!(
        acquired.fencing_token > holder.fencing_token,
        "TTL takeover must advance the process fencing token"
    );
    registry
        .complete_process_lease(&ProcessLeaseCompletion::from_lease(&acquired))
        .await
        .expect("release TTL successor lease");
}
