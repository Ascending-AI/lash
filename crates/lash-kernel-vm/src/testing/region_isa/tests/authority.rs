use super::*;

// D's missing affine transaction declarations stay inside this test module.
// The published state contains the whole fixture observation, not just money.
pub(in super::super) struct FakeLedger {
    lineage: u64,
    revision: u64,
    lease: u64,
    published: PrefixSnapshot,
}
struct FakeLease {
    lineage: u64,
    revision: u64,
    lease: u64,
}
struct FakeDraft {
    token: FakeLease,
    dependencies: Vec<(Identity, u64, u64)>,
    next: PrefixSnapshot,
}

#[derive(Debug, PartialEq, Eq)]
pub(in super::super) enum Refusal {
    Foreign,
    Stale,
    Sibling,
    Dependency,
    BeforePrepare,
    BeforeCommit,
}

impl FakeLedger {
    pub(in super::super) fn new(published: PrefixSnapshot) -> Self {
        Self {
            lineage: 7,
            revision: 3,
            lease: 0,
            published,
        }
    }
    pub(in super::super) fn snapshot(&self) -> &PrefixSnapshot {
        &self.published
    }
    pub(in super::super) fn publish(
        &mut self,
        next: PrefixSnapshot,
        fault: Option<Interruption>,
    ) -> Result<(), Refusal> {
        let token = self.acquire();
        self.prepare(token, next, fault)
            .and_then(|draft| self.commit(draft, fault))
    }

    fn acquire(&mut self) -> FakeLease {
        self.lease += 1;
        FakeLease {
            lineage: self.lineage,
            revision: self.revision,
            lease: self.lease,
        }
    }
    fn authenticate(&self, token: &FakeLease) -> Result<(), Refusal> {
        if token.lineage != self.lineage {
            return Err(Refusal::Foreign);
        }
        if token.revision != self.revision {
            return Err(Refusal::Stale);
        }
        if token.lease != self.lease {
            return Err(Refusal::Sibling);
        }
        Ok(())
    }
    fn prepare(
        &self,
        token: FakeLease,
        next: PrefixSnapshot,
        fault: Option<Interruption>,
    ) -> Result<FakeDraft, Refusal> {
        self.authenticate(&token)?;
        if fault == Some(Interruption::BeforePrepareFailure) {
            return Err(Refusal::BeforePrepare);
        }
        Ok(FakeDraft {
            token,
            dependencies: self.published.facts.dependencies.clone(),
            next,
        })
    }
    fn commit(&mut self, draft: FakeDraft, fault: Option<Interruption>) -> Result<(), Refusal> {
        self.authenticate(&draft.token)?;
        if draft.dependencies != self.published.facts.dependencies {
            return Err(Refusal::Dependency);
        }
        if fault == Some(Interruption::BeforeCommitFailure) {
            return Err(Refusal::BeforeCommit);
        }
        self.published = draft.next;
        self.revision += 1;
        Ok(())
    }
}

/// ISA-AUTHORITY-ATOMIC: a foreign lineage, stale stamp, sibling preparation or
/// changed dependency must refuse before any value, graph or charge is installed.
#[test]
fn fake_authority_refuses_foreign_stale_and_sibling_publications_atomically() {
    let expected = source_oracle_fixture(&roomy());
    let initial = expected[0].clone();
    let next = expected[2].clone();
    let fresh = || FakeLedger {
        lineage: 7,
        revision: 3,
        lease: 0,
        published: initial.clone(),
    };
    let mut ledger = fresh();
    let mut foreign = fresh();
    foreign.lineage = 8;
    let token = foreign.acquire();
    assert_eq!(
        ledger.prepare(token, next.clone(), None).err(),
        Some(Refusal::Foreign)
    );
    assert_eq!(ledger.published, initial);

    let stale = ledger.acquire();
    ledger.revision += 1;
    assert_eq!(
        ledger.prepare(stale, next.clone(), None).err(),
        Some(Refusal::Stale)
    );
    assert_eq!(ledger.published, initial);

    let token = ledger.acquire();
    let draft = ledger.prepare(token, next.clone(), None).ok().unwrap();
    let _sibling = ledger.acquire();
    assert_eq!(ledger.commit(draft, None), Err(Refusal::Sibling));
    assert_eq!(ledger.published, initial);

    let token = ledger.acquire();
    let draft = ledger.prepare(token, next.clone(), None).ok().unwrap();
    ledger.published.facts.dependencies[1].2 += 1;
    let before = ledger.published.clone();
    assert_eq!(ledger.commit(draft, None), Err(Refusal::Dependency));
    assert_eq!(ledger.published, before);

    for fault in [
        Interruption::BeforePrepareFailure,
        Interruption::BeforeCommitFailure,
    ] {
        let mut ledger = fresh();
        let token = ledger.acquire();
        let result = ledger
            .prepare(token, next.clone(), Some(fault))
            .and_then(|draft| ledger.commit(draft, Some(fault)));
        assert_eq!(
            result,
            Err(if fault == Interruption::BeforePrepareFailure {
                Refusal::BeforePrepare
            } else {
                Refusal::BeforeCommit
            })
        );
        assert_eq!(ledger.published, initial);
        assert_eq!(ledger.revision, 3);
        // An actual committed control exercises all nonempty facts, including
        // graph retention, value provenance and the two ordered charges.
        let token = ledger.acquire();
        let draft = ledger.prepare(token, next.clone(), None).ok().unwrap();
        ledger.commit(draft, None).unwrap();
        assert_eq!(ledger.published, next);
        assert_eq!(ledger.revision, 4);
    }
}
