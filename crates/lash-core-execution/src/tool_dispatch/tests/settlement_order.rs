use super::*;

struct ScheduledProbe {
    index: usize,
    name: &'static str,
    release: oneshot::Receiver<()>,
}

#[tokio::test]
async fn scheduler_runs_every_item_concurrently_and_preserves_order() {
    let (slow_release, slow_gate) = oneshot::channel();
    let (formerly_serial_release, formerly_serial_gate) = oneshot::channel();
    let (fast_release, fast_gate) = oneshot::channel();
    let probes = vec![
        ScheduledProbe {
            index: 0,
            name: "slow",
            release: slow_gate,
        },
        ScheduledProbe {
            index: 1,
            name: "formerly_serial",
            release: formerly_serial_gate,
        },
        ScheduledProbe {
            index: 2,
            name: "fast",
            release: fast_gate,
        },
    ];
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();

    let scheduled = schedule_tool_batch(probes, |probe| probe.index, {
        let entered_tx = entered_tx.clone();
        move |probe| {
            let entered_tx = entered_tx.clone();
            let completed_tx = completed_tx.clone();
            async move {
                entered_tx
                    .send(probe.name)
                    .expect("scheduler observer remains alive");
                probe.release.await.expect("observer releases every call");
                completed_tx
                    .send(probe.name)
                    .expect("scheduler observer remains alive");
                probe.name
            }
        }
    });
    drop(entered_tx);

    let observe_concurrency = async {
        let mut entered = [
            entered_rx.recv().await.expect("first call entered"),
            entered_rx.recv().await.expect("second call entered"),
            entered_rx.recv().await.expect("third call entered"),
        ];
        entered.sort_unstable();
        assert_eq!(
            entered,
            ["fast", "formerly_serial", "slow"],
            "every call, including the formerly-serial tool, must enter before any call completes"
        );

        fast_release.send(()).expect("fast call remains gated");
        assert_eq!(completed_rx.recv().await, Some("fast"));
        formerly_serial_release
            .send(())
            .expect("formerly-serial call remains gated");
        assert_eq!(completed_rx.recv().await, Some("formerly_serial"));
        slow_release.send(()).expect("slow call remains gated");
        assert_eq!(completed_rx.recv().await, Some("slow"));
    };

    let (scheduled, ()) = tokio::join!(scheduled, observe_concurrency);
    let outputs = scheduled.outcomes;
    assert_eq!(
        scheduled.settlement_order,
        vec![2, 1, 0],
        "the recorded settlement order is completion order, fastest first"
    );
    assert_eq!(
        outputs,
        ["slow", "formerly_serial", "fast"],
        "returned outcomes preserve the caller's fixed input order, not completion order"
    );
}
