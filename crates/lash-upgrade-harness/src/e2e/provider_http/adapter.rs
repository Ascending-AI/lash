//! The pinned provider seam over the recorded socket fixture.

use std::time::Duration;

use anyhow::{Result, anyhow, ensure};

use super::{HttpReceipt, RecordedHttpFixture, TransportEvent, transcript::HttpTranscript};
use crate::e2e::{
    Step,
    case::CaseLease,
    control::{CleanupReceipt, WorkIdentity},
    provider::{ProviderFixture, ProviderReceipt, ProviderRequest, ProviderResponse},
};

pub struct StrictHttpProvider {
    transcript: HttpTranscript,
    work: Vec<WorkIdentity>,
    port_index: usize,
    running: Option<RecordedHttpFixture>,
    finished: Option<HttpReceipt>,
}

impl StrictHttpProvider {
    /// Identities are bound from admitted work before the fixture boots.
    /// This correspondence is an expected request, not journal evidence.
    pub fn new(
        transcript: HttpTranscript,
        work: Vec<WorkIdentity>,
        port_index: usize,
    ) -> Result<Self> {
        transcript.validate()?;
        ensure!(
            work.len() == transcript.occurrences.len(),
            "one work identity per HTTP occurrence is required"
        );
        Ok(Self {
            transcript,
            work,
            port_index,
            running: None,
            finished: None,
        })
    }

    pub fn fixture(&self) -> Result<&RecordedHttpFixture> {
        self.running
            .as_ref()
            .ok_or_else(|| anyhow!("recorded provider is not serving"))
    }

    pub fn receipt(&self) -> Result<HttpReceipt> {
        match &self.finished {
            Some(receipt) => Ok(receipt.clone()),
            None => self.fixture()?.receipt(),
        }
    }
}

impl ProviderFixture for StrictHttpProvider {
    fn boot<'a>(&'a mut self, lease: &'a mut CaseLease) -> Step<'a, String> {
        Box::pin(async move {
            ensure!(
                self.running.is_none() && self.finished.is_none(),
                "provider fixture already booted"
            );
            let port = *lease
                .ports
                .get(self.port_index)
                .ok_or_else(|| anyhow!("provider has no leased port"))?;
            let fixture = RecordedHttpFixture::start(
                ([127, 0, 0, 1], port).into(),
                self.transcript.clone(),
                &lease.directory.join("outside-effects.jsonl"),
            )
            .await?;
            let url = fixture.base_url();
            self.running = Some(fixture);
            Ok(url)
        })
    }

    fn receipts(&self) -> Result<Vec<ProviderReceipt>> {
        let receipt = self.receipt()?;
        let mut receipts = Vec::new();
        for (index, occurrence) in self.transcript.occurrences.iter().enumerate() {
            if !receipt.events.iter().any(|event| matches!(event,
                TransportEvent::RequestMatched { occurrence: identity } if identity == &occurrence.identity
            )) {
                continue;
            }
            receipts.push(ProviderReceipt {
                request: ProviderRequest {
                    occurrence: occurrence.identity.clone(),
                    work: self.work[index].clone(),
                    request: occurrence.body.clone(),
                },
                response: ProviderResponse {
                    status: occurrence.response.status,
                    headers: occurrence.response.headers.iter().cloned().collect(),
                    frames: occurrence
                        .response
                        .chunks
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| {
                            receipt.events.iter().any(|event| {
                                matches!(event,
                                    TransportEvent::ChunkWritten { occurrence: identity, chunk }
                                        if identity == &occurrence.identity && chunk == index
                                )
                            })
                        })
                        .map(|(_, chunk)| chunk.bytes.as_bytes().to_vec())
                        .collect(),
                    usage: occurrence.response.usage.clone(),
                    typed_failure: occurrence.response.typed_failure.clone(),
                },
            });
        }
        Ok(receipts)
    }

    fn finish(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            let fixture = self
                .running
                .take()
                .ok_or_else(|| anyhow!("provider fixture is not serving"))?;
            let receipt = tokio::time::timeout(Duration::from_secs(10), fixture.finish()).await??;
            let verified = receipt.verify();
            self.finished = Some(receipt);
            verified?;
            Ok(vec![CleanupReceipt {
                resource: "recorded provider/effect listener and connection tasks".into(),
                closed: true,
                detail: "listener closed; all connections joined; strict request counts reconciled"
                    .into(),
            }])
        })
    }
}
