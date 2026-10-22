use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts live instances, so a test can prove every value is dropped
/// exactly once.
#[derive(Debug)]
struct Tracked {
    value: usize,
    live: Arc<AtomicUsize>,
}

impl Tracked {
    fn new(value: usize, live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::SeqCst);
        Self {
            value,
            live: Arc::clone(live),
        }
    }
}

impl crate::allocation::SourceCapacity for Tracked {
    fn owned_capacity(&self) -> u64 {
        0
    }
}

impl Clone for Tracked {
    fn clone(&self) -> Self {
        Self::new(self.value, &self.live)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

fn values(vec: &AppendVec<usize>) -> Vec<usize> {
    vec.to_vec()
}

#[test]
fn a_held_snapshot_shares_the_buffer_and_never_sees_later_appends() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]).unwrap();
    let held = writer.clone();
    writer.extend([4, 5]).unwrap();

    assert_eq!(values(&held), [1, 2, 3]);
    assert_eq!(values(&writer), [1, 2, 3, 4, 5]);
    assert!(
        std::ptr::eq(held.as_ptr(), writer.as_ptr()),
        "appends past a held snapshot write the shared buffer in place"
    );
}

#[test]
fn the_append_after_a_batch_that_grew_the_buffer_writes_in_place() {
    let mut writer = AppendVec::new();
    writer.extend(0..100).unwrap();
    let held = writer.clone();
    writer.push(100).unwrap();

    assert_eq!(values(&held), (0..100).collect::<Vec<_>>());
    assert!(
        std::ptr::eq(held.as_ptr(), writer.as_ptr()),
        "a batch leaves room, so the next append does not copy it"
    );
}

#[test]
fn a_handle_behind_the_tip_forks_on_a_different_value() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2]).unwrap();
    let mut other = writer.clone();
    writer.push(3).unwrap();
    other.push(9).unwrap();

    assert_eq!(values(&writer), [1, 2, 3]);
    assert_eq!(values(&other), [1, 2, 9]);
    assert!(!std::ptr::eq(writer.as_ptr(), other.as_ptr()));
}

#[test]
fn a_handle_behind_the_tip_adopts_the_same_value() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2]).unwrap();
    let mut other = writer.clone();
    writer.extend([3, 4]).unwrap();
    other
        .extend_adopting([3, 4, 5], |existing, value| existing == value)
        .unwrap();

    assert_eq!(values(&writer), [1, 2, 3, 4]);
    assert_eq!(values(&other), [1, 2, 3, 4, 5]);
    assert!(
        std::ptr::eq(writer.as_ptr(), other.as_ptr()),
        "adopted slots and the append after them stay in the shared buffer"
    );
}

#[test]
fn replace_from_copies_when_another_handle_saw_a_slot() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]).unwrap();
    let held = writer.clone();

    writer.replace_from(2, [30, 40]).unwrap();

    assert_eq!(values(&held), [1, 2, 3], "the held snapshot never changes");
    assert_eq!(values(&writer), [1, 2, 30, 40]);
    assert!(!std::ptr::eq(held.as_ptr(), writer.as_ptr()));
}

#[test]
fn replace_from_with_fewer_values_shortens_the_view() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3, 4]).unwrap();
    writer.replace_from(1, [20]).unwrap();
    assert_eq!(values(&writer), [1, 20]);
    writer.push(5).unwrap();
    assert_eq!(values(&writer), [1, 20, 5]);
}

#[test]
fn an_adopter_sees_the_slot_so_the_tip_no_longer_edits_it_in_place() {
    let mut writer = AppendVec::with_capacity(8);
    writer.push(1).unwrap();
    let mut other = writer.clone();
    writer.push(2).unwrap();
    other
        .push_adopting(2, |existing, value| existing == value)
        .unwrap();

    writer.replace_from(1, [20]).unwrap();

    assert_eq!(values(&other), [1, 2], "the adopted slot never changes");
    assert_eq!(values(&writer), [1, 20]);
}

#[test]
fn make_mut_copies_a_shared_buffer_and_edits_a_sole_one_in_place() {
    let mut writer = AppendVec::from(vec![1, 2, 3]);
    let held = writer.clone();
    writer.make_mut().unwrap()[0] = 10;
    assert_eq!(values(&held), [1, 2, 3]);
    assert_eq!(values(&writer), [10, 2, 3]);

    let before = writer.as_ptr();
    writer.make_mut().unwrap()[1] = 20;
    assert!(std::ptr::eq(before, writer.as_ptr()));
    assert_eq!(values(&writer), [10, 20, 3]);
}

#[test]
fn every_value_is_dropped_exactly_once() {
    let live = Arc::new(AtomicUsize::new(0));
    // A failed graph draft can leave an unpublished tail in a shared buffer.
    // Dropping its last observer must release that tail even while a reader
    // keeps the published prefix alive.
    {
        let mut prefix = AppendVec::with_capacity(4);
        prefix.push(Tracked::new(0, &live)).unwrap();
        let mut draft = prefix.clone();
        draft.push(Tracked::new(1, &live)).unwrap();
        assert_eq!(live.load(Ordering::SeqCst), 2);
        drop(draft);
        assert_eq!(
            live.load(Ordering::SeqCst),
            1,
            "an unpublished tail has no remaining reader"
        );
        assert_eq!(prefix[0].value, 0);
    }
    assert_eq!(live.load(Ordering::SeqCst), 0);
    {
        let mut writer = AppendVec::new();
        let mut held = Vec::new();
        for value in 0..64 {
            writer.push(Tracked::new(value, &live)).unwrap();
            if value % 5 == 0 {
                held.push(writer.clone());
            }
        }
        let mut fork = held[3].clone();
        fork.push(Tracked::new(1000, &live)).unwrap();
        writer.replace_from(60, [Tracked::new(600, &live)]).unwrap();
        let mut truncated = writer.clone();
        truncated.truncate(10);
        truncated.make_mut().unwrap()[0] = Tracked::new(7, &live);
        assert_eq!(writer.len(), 61);
        assert_eq!(writer[60].value, 600);
        assert_eq!(fork.last().map(|tracked| tracked.value), Some(1000));
    }
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_appenders_from_one_snapshot_each_see_only_their_own_values() {
    let mut base = AppendVec::with_capacity(4096);
    base.extend(0..16usize).unwrap();
    let threads = (0..8usize)
        .map(|thread| {
            let mut handle = base.clone();
            std::thread::spawn(move || {
                for step in 0..200 {
                    handle.push(thread * 1000 + step).unwrap();
                }
                handle
            })
        })
        .collect::<Vec<_>>();
    for (thread, joined) in threads.into_iter().enumerate() {
        let handle = joined.join().expect("appender thread");
        assert_eq!(&handle[..16], (0..16).collect::<Vec<_>>().as_slice());
        assert_eq!(
            &handle[16..],
            (0..200)
                .map(|step| thread * 1000 + step)
                .collect::<Vec<_>>()
                .as_slice()
        );
    }
    assert_eq!(values(&base), (0..16).collect::<Vec<_>>());
}

#[test]
fn concurrent_readers_of_held_snapshots_race_a_writer_safely() {
    let mut writer = AppendVec::new();
    writer.extend(0..8usize).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel::<AppendVec<usize>>();
    let reader = std::thread::spawn(move || {
        let mut checked = 0;
        for snapshot in receiver {
            assert!(snapshot.iter().copied().eq(0..snapshot.len()));
            checked += 1;
        }
        checked
    });
    for value in 8..2000 {
        writer.push(value).unwrap();
        if value % 3 == 0 {
            sender.send(writer.clone()).expect("reader alive");
        }
    }
    drop(sender);
    assert!(reader.join().expect("reader thread") > 0);
}

/// LO-EVICT-SOURCE: source slots and handle-registry capacity are admitted
/// before allocation, and the last snapshot owns their release.
#[test]
fn source_buffer_capacity_is_reserved_before_allocation_and_held_by_readers() {
    use crate::allocation::{
        AllocationBudget, AllocationError, AllocationHold, AllocationLedger, AllocationReservation,
        ReserveError,
    };
    use std::sync::atomic::AtomicU64;
    #[derive(Clone)]
    struct Ledger {
        used: Arc<AtomicU64>,
        limit: Arc<AtomicU64>,
    }
    struct Hold {
        ledger: Ledger,
        bytes: u64,
    }
    impl Drop for Hold {
        fn drop(&mut self) {
            self.ledger.used.fetch_sub(self.bytes, Ordering::SeqCst);
        }
    }
    impl AllocationReservation for Hold {
        fn retain(&self) {}
    }
    impl AllocationLedger for Ledger {
        fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
            let limit = self.limit.load(Ordering::SeqCst);
            if bytes > limit {
                return Err(AllocationError {
                    kind: ReserveError::TooLarge,
                    actor: "source-law".into(),
                    requested: bytes,
                    capacity: limit,
                });
            }
            self.used
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    used.checked_add(bytes).filter(|total| *total <= limit)
                })
                .map_err(|_| AllocationError {
                    kind: ReserveError::Pressure,
                    actor: "source-law".into(),
                    requested: bytes,
                    capacity: limit,
                })?;
            Ok(AllocationHold::new(Hold {
                ledger: self.clone(),
                bytes,
            }))
        }
        fn reserve_shared(&self, _: usize, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.reserve(bytes)
        }
    }
    let ledger = Ledger {
        used: Arc::new(AtomicU64::new(0)),
        limit: Arc::new(AtomicU64::new(1 << 20)),
    };
    let budget = AllocationBudget::new(ledger.clone());
    let mut writer = AppendVec::<usize>::try_with_capacity(8, &budget).unwrap();
    let charged = ledger.used.load(Ordering::SeqCst);
    assert!(
        charged >= 8 * std::mem::size_of::<usize>() as u64,
        "source allocation must not precede its reservation"
    );
    writer.extend(0..4).unwrap();
    let held = writer.clone();
    writer.truncate(2);
    assert_eq!(
        ledger.used.load(Ordering::SeqCst),
        charged,
        "clones and handle lengths share the pre-admitted registry"
    );
    drop(writer);
    assert_eq!(held.as_slice(), &[0, 1, 2, 3]);
    assert_eq!(ledger.used.load(Ordering::SeqCst), charged);
    drop(held);
    assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
    let live = Arc::new(AtomicUsize::new(0));
    let mut writer = AppendVec::try_with_capacity(4, &budget).unwrap();
    for value in 0..4 {
        writer.push(Tracked::new(value, &live)).unwrap();
    }
    let held = writer.clone();
    let old_charge = ledger.used.load(Ordering::SeqCst);
    let next_charge = AppendVec::<Tracked>::buffer_capacity_bytes(8);
    ledger
        .limit
        .store(old_charge + next_charge - 1, Ordering::SeqCst);
    assert!(matches!(
        writer.push(Tracked::new(4, &live)),
        Err(AllocationError {
            kind: ReserveError::Pressure,
            ..
        })
    ));
    assert_eq!(writer.len(), 4);
    assert_eq!(
        live.load(Ordering::SeqCst),
        4,
        "denied growth cloned no source values"
    );
    assert!(AppendVec::ptr_eq(&writer, &held));
    assert_eq!(ledger.used.load(Ordering::SeqCst), old_charge);
    ledger.limit.store(1 << 20, Ordering::SeqCst);
    writer.push(Tracked::new(4, &live)).unwrap();
    assert_eq!(held.len(), 4);
    assert_eq!(writer.len(), 5);
    assert_eq!(ledger.used.load(Ordering::SeqCst), old_charge + next_charge);
    drop(held);
    assert_eq!(ledger.used.load(Ordering::SeqCst), next_charge);
    drop(writer);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
    assert!(matches!(
        AppendVec::<usize>::try_with_capacity(1 << 20, &budget),
        Err(AllocationError {
            kind: ReserveError::TooLarge,
            ..
        })
    ));
    assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
    // An admitted producer remains charged while its destructor runs, even
    // when the following buffer-growth admission refuses.
    #[derive(Clone)]
    struct DropWitness {
        used: Arc<AtomicU64>,
        observed: Arc<AtomicU64>,
    }
    impl crate::allocation::SourceCapacity for DropWitness {
        fn owned_capacity(&self) -> u64 {
            4096
        }
    }
    impl Drop for DropWitness {
        fn drop(&mut self) {
            self.observed
                .store(self.used.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }
    ledger.limit.store(1 << 20, Ordering::SeqCst);
    let mut empty = AppendVec::try_with_capacity(0, &budget).unwrap();
    let baseline = ledger.used.load(Ordering::SeqCst);
    let observed = Arc::new(AtomicU64::new(0));
    ledger.limit.store(baseline + 4096, Ordering::SeqCst);
    assert!(
        empty
            .push(DropWitness {
                used: ledger.used.clone(),
                observed: observed.clone()
            })
            .is_err()
    );
    assert!(
        observed.load(Ordering::SeqCst) >= baseline + 4096,
        "the copied fields stay admitted until after the refused value is destroyed"
    );
    drop(empty);
    ledger.limit.store(1 << 20, Ordering::SeqCst);
    #[derive(Debug)]
    struct OwnedSource {
        body: String,
        copies: Arc<AtomicUsize>,
    }
    impl Clone for OwnedSource {
        fn clone(&self) -> Self {
            self.copies.fetch_add(1, Ordering::SeqCst);
            Self {
                body: self.body.clone(),
                copies: self.copies.clone(),
            }
        }
    }
    impl crate::allocation::SourceCapacity for OwnedSource {
        fn owned_capacity(&self) -> u64 {
            self.body.capacity() as u64
        }
    }
    ledger.limit.store(1 << 20, Ordering::SeqCst);
    let copies = Arc::new(AtomicUsize::new(0));
    let mut writer = AppendVec::try_with_capacity(1, &budget).unwrap();
    writer
        .push(OwnedSource {
            body: "x".repeat(4096),
            copies: copies.clone(),
        })
        .unwrap();
    let reader = writer.clone();
    let held = ledger.used.load(Ordering::SeqCst);
    let slots = AppendVec::<OwnedSource>::buffer_capacity_bytes(4);
    ledger.limit.store(held + slots + 4095, Ordering::SeqCst);
    assert!(
        matches!(
            writer.push(OwnedSource {
                body: String::new(),
                copies: copies.clone()
            }),
            Err(AllocationError {
                kind: ReserveError::Pressure,
                ..
            })
        ),
        "detaching must admit copied owned fields as well as inline slots"
    );
    assert_eq!(
        copies.load(Ordering::SeqCst),
        0,
        "a refused detach copies no source field"
    );
    assert!(AppendVec::ptr_eq(&reader, &writer));
    assert_eq!(reader[0].body.len(), 4096);
    drop(writer);
    drop(reader);
    assert_eq!(ledger.used.load(Ordering::SeqCst), 0);
}
