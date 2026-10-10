use super::*;
use crate::domain::CellId;

fn id(byte: u8) -> NodeId {
    NodeId([byte; 32])
}

fn session() -> SessionId {
    SessionId::try_from("s1".to_owned()).unwrap()
}

fn turn() -> TurnId {
    TurnId::try_from("t1".to_owned()).unwrap()
}

fn exec() -> ExecKey {
    ExecKey::Cell(session(), turn(), CellId::new("c1"))
}

/// A commit's publications are exactly the projection of its graph writes,
/// in order: a missing, extra or reordered change is refused, and a range
/// prune projects no change.
#[test]
fn publications_must_be_exactly_the_projection_of_the_writes() {
    let writes = vec![
        GraphDomainWrite::ExecRoot(ExecRootWrite::Put {
            exec: exec(),
            expected: Some(SnapshotRev(1)),
            expected_root: Some(id(1)),
            root: id(2),
            commit_id: CommitId([7; 32]),
        }),
        GraphDomainWrite::RunFact(RunFactWrite::Prune {
            owner: OwnerKey::Turn(session(), turn()),
            before: RunSeq(3),
        }),
        GraphDomainWrite::ReleasePin(ReleasePinWrite::Retire {
            release: "1.0".into(),
            expected: id(3),
        }),
    ];
    let projected = vec![
        RootChange::Put {
            owner: RootOwner::Exec(exec()),
            expected: Some(id(1)),
            root: id(2),
        },
        RootChange::Retire {
            owner: RootOwner::ReleasePin {
                release: "1.0".into(),
            },
            expected: id(3),
        },
    ];
    let mut graph = PreparedGraph {
        publications: projected.clone(),
        ..PreparedGraph::default()
    };
    assert_eq!(graph.check_projection(&writes), Ok(()));
    graph.publications.reverse();
    assert_eq!(
        graph.check_projection(&writes),
        Err(GraphRefusal::PublicationMismatch)
    );
    graph.publications = projected[..1].to_vec();
    assert_eq!(
        graph.check_projection(&writes),
        Err(GraphRefusal::PublicationMismatch)
    );

    let expected = OwnerRevisionSet::new()
        .expect(RootOwner::Exec(exec()), Some(1))
        .expect(
            RootOwner::ReleasePin {
                release: "1.0".into(),
            },
            None,
        );
    let advanced = expected.advanced().unwrap();
    assert_eq!(advanced.get(&RootOwner::Exec(exec())), Some(Some(2)));
    assert_eq!(
        advanced.get(&RootOwner::ReleasePin {
            release: "1.0".into()
        }),
        Some(Some(1))
    );
}
