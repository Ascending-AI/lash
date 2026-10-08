//! Laws of a session's mail as its senders see it (ADR 0132 §3, FIG-5196):
//! the session actor is the only driver of its session's work, takes each
//! input once and in arrival order, and runs one turn at a time; a handle is
//! only a reader of what its input's run answers.

use super::*;

/// A provider that answers `echo: <last user text>`, holding the answer to
/// [`HELD`] until `release` is notified. It records each text it was asked
/// to answer, in order, and the most calls it ever had in flight at once.
struct Witness {
    release: Arc<Notify>,
    asked: StdMutex<Vec<String>>,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

impl Witness {
    fn provider(self: &Arc<Self>) -> ProviderHandle {
        let witness = Arc::clone(self);
        crate::testing::TestProvider::builder()
            .kind("session-mail-arrival")
            .complete(move |request| {
                let witness = Arc::clone(&witness);
                async move {
                    let now = witness.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    witness.most_in_flight.fetch_max(now, Ordering::SeqCst);
                    let text = last_user_text(&request);
                    witness.asked.lock_recover().push(text.clone());
                    if text.contains(HELD) {
                        witness.release.notified().await;
                    }
                    witness.in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(text_response(&format!("echo: {text}")))
                }
            })
            .build()
            .into_handle()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock_recover().clone()
    }
}

async fn witnessed_core() -> Result<(LashCore, Arc<Witness>)> {
    let witness = Arc::new(Witness {
        release: Arc::new(Notify::new()),
        asked: StdMutex::new(Vec::new()),
        in_flight: AtomicUsize::new(0),
        most_in_flight: AtomicUsize::new(0),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(witness.provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    Ok((core, witness))
}

/// Wait until the witness has been asked `count` texts.
async fn asked(witness: &Witness, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while witness.asked.lock_recover().len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the provider is asked");
}

/// An idle session's send runs at once. Sends that arrive while a turn
/// runs each run once, after it, in the order they arrived, one turn at a
/// time; a send repeated under a queued input's id is that input, not a
/// second one. Inputs on both sides of the actor's idle pass each run once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sends_run_once_each_in_arrival_order_one_at_a_time() -> Result<()> {
    let (core, witness) = witnessed_core().await?;
    let session = core
        .session(crate::SessionId::parse("arrival-order").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let idle = session.send(TurnInput::text("first")).output().await?;
    assert_eq!(idle.assistant_message(), Some("echo: first"));

    let held = session.send(TurnInput::text(HELD)).await?;
    asked(&witness, 2).await;
    let mut queued = Vec::new();
    for text in ["second", "third", "fourth"] {
        queued.push(
            session
                .send(TurnInput::text(text))
                .id(crate::TurnId::parse(format!("{text}-input")).expect("nonblank host identity"))
                .await?,
        );
    }
    let repeated = session
        .send(TurnInput::text("third"))
        .id(crate::TurnId::parse("third-input").expect("nonblank host identity"))
        .await?;
    assert_eq!(
        repeated.input_id(),
        queued[1].input_id(),
        "a repeated id names the queued input"
    );
    witness.release.notify_one();

    assert_eq!(
        held.output().await?.assistant_message(),
        Some(format!("echo: {HELD}").as_str())
    );
    for (handle, text) in queued.into_iter().zip(["second", "third", "fourth"]) {
        assert_eq!(
            handle.output().await?.assistant_message(),
            Some(format!("echo: {text}").as_str()),
            "each send answers its own input's run"
        );
    }
    assert_eq!(
        repeated.output().await?.assistant_message(),
        Some("echo: third")
    );
    assert_eq!(
        witness.asked(),
        ["first", HELD, "second", "third", "fourth"],
        "each input runs once, in arrival order"
    );
    assert_eq!(
        witness.most_in_flight.load(Ordering::SeqCst),
        1,
        "one turn at a time"
    );
    assert_eq!(session.durable().turn_input_applications().await?.len(), 5);
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// A handle reads its input's run; it does not drive it. A send whose
/// handle is dropped while its turn runs still runs to its commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_send_handle_stops_nothing() -> Result<()> {
    let (core, witness) = witnessed_core().await?;
    let session = core
        .session(crate::SessionId::parse("dropped-handle").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session.send(TurnInput::text(HELD)).await?;
    let input = handle.input_id().clone();
    asked(&witness, 1).await;
    drop(handle);
    witness.release.notify_one();

    let output = session.attach(input).output().await?;
    assert_eq!(
        output.assistant_message(),
        Some(format!("echo: {HELD}").as_str())
    );
    assert!(
        committed(&session)
            .await
            .messages()
            .iter()
            .any(|message| crate::message_text(message) == format!("echo: {HELD}")),
        "the dropped send's reply is committed"
    );
    Ok(())
}
