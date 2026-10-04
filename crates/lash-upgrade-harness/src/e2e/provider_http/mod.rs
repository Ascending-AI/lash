//! Strict recorded provider HTTP and a persistent outside-effect service.
//! This is socket plumbing and an external oracle, not a journal substitute.

mod adapter;
pub mod ledger;
pub mod node_host;
pub mod scenarios;
pub mod transcript;
pub(crate) mod wire;

pub use adapter::StrictHttpProvider;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::{JoinHandle, JoinSet};

use ledger::{EffectAcceptance, EffectDelivery, EffectLedger};
use transcript::{HttpTranscript, RecordedResponse, StreamEnd};

/// These phases describe the fixture's transport. Durable A/X/D/V facts
/// must come from the controller's independently decoded journal evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TransportEvent {
    RequestMatched {
        occurrence: String,
    },
    ChunkWritten {
        occurrence: String,
        chunk: usize,
    },
    BarrierEntered {
        occurrence: String,
        barrier: String,
    },
    ResponseEnded {
        occurrence: String,
        termination: StreamEnd,
    },
    EffectAccepted {
        acceptance: EffectAcceptance,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpReceipt {
    pub transcript: String,
    pub expected: usize,
    pub matched: usize,
    pub ended: usize,
    pub events: Vec<TransportEvent>,
    pub effects: Vec<EffectAcceptance>,
    pub mutations: usize,
    pub violations: Vec<String>,
}

struct State {
    transcript: HttpTranscript,
    next: usize,
    ended: usize,
    events: Vec<TransportEvent>,
    released: BTreeSet<String>,
    effect_gates: BTreeMap<(String, u32), (String, StreamEnd)>,
    violations: Vec<String>,
    ledger: EffectLedger,
}

struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

impl Shared {
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("HTTP fixture state panicked"))
    }

    fn event(&self, event: TransportEvent) -> Result<()> {
        self.state()?.events.push(event);
        self.changed.notify_waiters();
        Ok(())
    }

    fn violation(&self, error: String) {
        if let Ok(mut state) = self.state() {
            state.violations.push(error);
        }
        self.changed.notify_waiters();
    }
}

/// One owned listener and every connection task. `finish` stops/reaps them
/// and reconciles counts; dropping the handle cannot supply passing proof.
pub struct RecordedHttpFixture {
    address: SocketAddr,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    serving: Option<JoinHandle<Result<()>>>,
}

impl RecordedHttpFixture {
    pub async fn start(
        bind: SocketAddr,
        transcript: HttpTranscript,
        ledger: &Path,
    ) -> Result<Self> {
        transcript.validate()?;
        let listener = TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                transcript,
                next: 0,
                ended: 0,
                events: Vec::new(),
                released: BTreeSet::new(),
                effect_gates: BTreeMap::new(),
                violations: Vec::new(),
                ledger: EffectLedger::open(ledger)?,
            }),
            changed: Notify::new(),
        });
        let (stop, stopped) = watch::channel(false);
        let serving = tokio::spawn(serve(listener, Arc::clone(&shared), stopped));
        Ok(Self {
            address,
            shared,
            stop,
            serving: Some(serving),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub fn effect_url(&self) -> String {
        format!("http://{}/effects", self.address)
    }

    /// Hold the outside service after synced acceptance, optionally losing
    /// the HTTP reply on release. This is an ambiguous external acceptance,
    /// not an X/D proposal or durable ACK barrier.
    pub fn hold_effect(
        &self,
        call: &str,
        attempt: u32,
        barrier: &str,
        reply: StreamEnd,
    ) -> Result<()> {
        let mut state = self.shared.state()?;
        ensure!(
            !call.is_empty() && attempt > 0 && !barrier.is_empty(),
            "effect gate has no identity"
        );
        ensure!(
            !state
                .ledger
                .deliveries()
                .iter()
                .any(|acceptance| acceptance.delivery.call_id == call
                    && acceptance.delivery.attempt == attempt),
            "effect gate was installed after acceptance"
        );
        ensure!(
            !state.effect_gates.contains_key(&(call.to_owned(), attempt)),
            "effect gate already installed"
        );
        ensure!(
            !state.effect_gates.values().any(|(held, _)| held == barrier)
                && !state
                    .transcript
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence
                        .response
                        .chunks
                        .iter()
                        .any(|chunk| chunk.hold_after.as_deref() == Some(barrier))),
            "duplicate transport barrier"
        );
        state
            .effect_gates
            .insert((call.to_owned(), attempt), (barrier.to_owned(), reply));
        Ok(())
    }

    pub fn receipt(&self) -> Result<HttpReceipt> {
        let state = self.shared.state()?;
        Ok(HttpReceipt {
            transcript: state.transcript.name.clone(),
            expected: state.transcript.occurrences.len(),
            matched: state.next,
            ended: state.ended,
            events: state.events.clone(),
            effects: state.ledger.deliveries().to_vec(),
            mutations: state.ledger.mutation_count(),
            violations: state.violations.clone(),
        })
    }

    /// Deadline-bound event predicate, with notification installed before
    /// reading state so a request cannot race the waiter into a missed wake.
    pub async fn wait_for(
        &self,
        deadline: Duration,
        predicate: impl Fn(&TransportEvent) -> bool,
    ) -> Result<TransportEvent> {
        tokio::time::timeout(deadline, async {
            loop {
                let changed = self.shared.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.shared.state()?;
                    ensure!(
                        state.violations.is_empty(),
                        "HTTP fixture violations: {:?}",
                        state.violations
                    );
                    if let Some(event) = state.events.iter().find(|event| predicate(event)) {
                        return Ok(event.clone());
                    }
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| anyhow!("HTTP fixture barrier missed its deadline"))?
    }

    /// Releases only a barrier whose bytes were actually written.
    pub fn release(&self, barrier: &str) -> Result<()> {
        let mut state = self.shared.state()?;
        ensure!(
            state.events.iter().any(|event| matches!(event,
                TransportEvent::BarrierEntered { barrier: entered, .. } if entered == barrier
            )),
            "transport barrier {barrier} has not been reached"
        );
        ensure!(
            state.released.insert(barrier.to_owned()),
            "transport barrier already released"
        );
        drop(state);
        self.shared.changed.notify_waiters();
        Ok(())
    }

    /// Always return the receipt, including first-failure artifacts. The
    /// caller must use `verify` before counting this fixture as passing.
    pub async fn finish(mut self) -> Result<HttpReceipt> {
        self.stop.send_replace(true);
        if let Some(serving) = self.serving.take() {
            serving.await??;
        }
        self.receipt()
    }
}

impl Drop for RecordedHttpFixture {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

impl HttpReceipt {
    pub fn verify(&self) -> Result<()> {
        ensure!(self.expected > 0, "HTTP fixture selected zero requests");
        ensure!(
            self.violations.is_empty(),
            "HTTP fixture violations: {:?}",
            self.violations
        );
        ensure!(
            self.matched == self.expected && self.ended == self.expected,
            "HTTP fixture counts expected={}, matched={}, ended={}",
            self.expected,
            self.matched,
            self.ended
        );
        Ok(())
    }
}

async fn serve(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) -> Result<()> {
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let shared = Arc::clone(&shared);
                let mut stop = stopped.clone();
                requests.spawn(async move {
                    tokio::select! {
                        result = tokio::time::timeout(Duration::from_secs(120), respond(stream, Arc::clone(&shared))) => {
                            match result {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => shared.violation(format!("{error:#}")),
                                Err(_) => shared.violation("HTTP connection exceeded fixture deadline".into()),
                            }
                        }
                        _ = stop.changed() => shared.violation("fixture stopped with an unfinished connection".into()),
                    }
                });
            }
            Some(result) = requests.join_next(), if !requests.is_empty() => {
                if let Err(error) = result {
                    shared.violation(format!("HTTP fixture connection panicked: {error}"));
                }
            }
        }
    }
    drop(listener);
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result {
            shared.violation(format!("HTTP fixture connection panicked: {error}"));
        }
    }
    Ok(())
}

async fn respond(mut stream: TcpStream, shared: Arc<Shared>) -> Result<()> {
    let request = wire::read(&mut stream).await?;
    if request.method == "POST" && request.path == "/effects" {
        let delivery: EffectDelivery = serde_json::from_value(request.body)?;
        let (acceptance, gate) = {
            let mut state = shared.state()?;
            let acceptance = state.ledger.accept(delivery)?;
            let gate = state.effect_gates.remove(&(
                acceptance.delivery.call_id.clone(),
                acceptance.delivery.attempt,
            ));
            (acceptance, gate)
        };
        shared.event(TransportEvent::EffectAccepted {
            acceptance: acceptance.clone(),
        })?;
        if let Some((barrier, reply)) = gate {
            hold(
                &shared,
                &format!(
                    "effect:{}:{}",
                    acceptance.delivery.call_id, acceptance.delivery.attempt
                ),
                &barrier,
            )
            .await?;
            if reply == StreamEnd::Disconnect {
                return Ok(());
            }
        }
        return wire::json(&mut stream, 200, &serde_json::to_value(acceptance)?).await;
    }
    let matched = match_request(&shared, &request);
    let (occurrence, response) = match matched {
        Ok(matched) => matched,
        Err(error) => {
            shared.violation(format!("{error:#}"));
            return wire::json(
                &mut stream,
                409,
                &serde_json::json!({ "error": "unexpected fixture request" }),
            )
            .await;
        }
    };
    shared.event(TransportEvent::RequestMatched {
        occurrence: occurrence.clone(),
    })?;
    wire::head(&mut stream, response.status, &response.headers).await?;
    for (chunk_index, chunk) in response.chunks.iter().enumerate() {
        wire::chunk(&mut stream, chunk.bytes.as_bytes()).await?;
        shared.event(TransportEvent::ChunkWritten {
            occurrence: occurrence.clone(),
            chunk: chunk_index,
        })?;
        if let Some(barrier) = &chunk.hold_after {
            hold(&shared, &occurrence, barrier).await?;
        }
    }
    if response.termination == StreamEnd::Complete {
        wire::end(&mut stream).await?;
    }
    // A Disconnect drops the socket without an HTTP end chunk. The client
    // sees the production transport's read failure after the recorded bytes.
    drop(stream);
    shared.state()?.ended += 1;
    shared.event(TransportEvent::ResponseEnded {
        occurrence,
        termination: response.termination,
    })
}

async fn hold(shared: &Shared, occurrence: &str, barrier: &str) -> Result<()> {
    shared.event(TransportEvent::BarrierEntered {
        occurrence: occurrence.into(),
        barrier: barrier.into(),
    })?;
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if shared.state()?.released.contains(barrier) {
            return Ok(());
        }
        changed.await;
    }
}

fn match_request(shared: &Shared, request: &wire::Request) -> Result<(String, RecordedResponse)> {
    let mut state = shared.state()?;
    let expected = state
        .transcript
        .occurrences
        .get(state.next)
        .ok_or_else(|| anyhow!("extra HTTP request after transcript exhausted"))?;
    ensure!(
        request.method == expected.method
            && request.path == expected.path
            && request.body == expected.body,
        "HTTP request {} differs from occurrence {}",
        state.next,
        expected.identity
    );
    let matched = (expected.identity.clone(), expected.response.clone());
    state.next += 1;
    Ok(matched)
}
