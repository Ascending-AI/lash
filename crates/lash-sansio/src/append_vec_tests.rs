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
    writer.extend([1, 2, 3]);
    let held = writer.clone();
    writer.extend([4, 5]);

    assert_eq!(values(&held), [1, 2, 3]);
    assert_eq!(values(&writer), [1, 2, 3, 4, 5]);
    assert!(
        std::ptr::eq(held.as_ptr(), writer.as_ptr()),
        "appends past a held snapshot write the shared buffer in place"
    );
}

#[test]
fn snapshots_across_many_appends_share_a_bounded_set_of_buffers() {
    let mut writer = AppendVec::new();
    let mut held = Vec::new();
    for value in 0..1000 {
        writer.push(value);
        held.push(writer.clone());
    }
    let buffers = held
        .iter()
        .map(|snapshot| snapshot.as_ptr())
        .collect::<std::collections::HashSet<_>>();
    // Doubling: one buffer per power of two, not one per snapshot.
    assert!(buffers.len() <= 12, "{} buffers", buffers.len());
    for (index, snapshot) in held.iter().enumerate() {
        assert_eq!(snapshot.len(), index + 1);
        assert_eq!(snapshot.last(), Some(&index));
    }
}

#[test]
fn a_handle_behind_the_tip_forks_on_a_different_value() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2]);
    let mut other = writer.clone();
    writer.push(3);
    other.push(9);

    assert_eq!(values(&writer), [1, 2, 3]);
    assert_eq!(values(&other), [1, 2, 9]);
    assert!(!std::ptr::eq(writer.as_ptr(), other.as_ptr()));
}

#[test]
fn a_handle_behind_the_tip_adopts_the_same_value() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2]);
    let mut other = writer.clone();
    writer.extend([3, 4]);
    other.extend_adopting([3, 4, 5], |existing, value| existing == value);

    assert_eq!(values(&writer), [1, 2, 3, 4]);
    assert_eq!(values(&other), [1, 2, 3, 4, 5]);
    assert!(
        std::ptr::eq(writer.as_ptr(), other.as_ptr()),
        "adopted slots and the append after them stay in the shared buffer"
    );
}

#[test]
fn ptr_eq_is_same_buffer_and_same_length() {
    let mut writer = AppendVec::from(vec![1, 2]);
    let held = writer.clone();
    assert!(AppendVec::ptr_eq(&held, &writer));
    writer.push(3);
    assert!(!AppendVec::ptr_eq(&held, &writer));
    assert!(!AppendVec::ptr_eq(
        &AppendVec::from(vec![1, 2]),
        &AppendVec::from(vec![1, 2])
    ));
}

#[test]
fn replace_from_edits_unseen_slots_in_place() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2]);
    let held = writer.clone();
    writer.extend([3, 4]);
    let before = writer.as_ptr();

    writer.replace_from(2, [30, 40]);

    assert_eq!(values(&held), [1, 2]);
    assert_eq!(values(&writer), [1, 2, 30, 40]);
    assert!(std::ptr::eq(before, writer.as_ptr()), "no copy");
}

#[test]
fn replace_from_copies_when_another_handle_saw_a_slot() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]);
    let held = writer.clone();

    writer.replace_from(2, [30, 40]);

    assert_eq!(values(&held), [1, 2, 3], "the held snapshot never changes");
    assert_eq!(values(&writer), [1, 2, 30, 40]);
    assert!(!std::ptr::eq(held.as_ptr(), writer.as_ptr()));
}

#[test]
fn a_dropped_reader_no_longer_blocks_an_in_place_edit() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]);
    let held = writer.clone();
    let reader = writer.clone();
    drop(reader);
    let before = writer.as_ptr();

    writer.replace_from(3, [4]);
    writer.replace_from(2, [30, 40]);

    assert_eq!(values(&held), [1, 2, 3]);
    assert_eq!(values(&writer), [1, 2, 30, 40]);
    assert!(
        !std::ptr::eq(before, writer.as_ptr()),
        "the live reader of slot 2 forces a copy"
    );

    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]);
    let short = {
        let mut short = writer.clone();
        short.truncate(1);
        short
    };
    drop(writer.clone());
    let before = writer.as_ptr();
    writer.replace_from(1, [20, 30]);
    assert_eq!(values(&short), [1]);
    assert_eq!(values(&writer), [1, 20, 30]);
    assert!(
        std::ptr::eq(before, writer.as_ptr()),
        "only live readers past the edit block it"
    );
}

#[test]
fn replace_from_with_fewer_values_shortens_the_view() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3, 4]);
    writer.replace_from(1, [20]);
    assert_eq!(values(&writer), [1, 20]);
    writer.push(5);
    assert_eq!(values(&writer), [1, 20, 5]);
}

#[test]
fn an_adopter_sees_the_slot_so_the_tip_no_longer_edits_it_in_place() {
    let mut writer = AppendVec::with_capacity(8);
    writer.push(1);
    let mut other = writer.clone();
    writer.push(2);
    other.push_adopting(2, |existing, value| existing == value);

    writer.replace_from(1, [20]);

    assert_eq!(values(&other), [1, 2], "the adopted slot never changes");
    assert_eq!(values(&writer), [1, 20]);
}

#[test]
fn make_mut_copies_a_shared_buffer_and_edits_a_sole_one_in_place() {
    let mut writer = AppendVec::from(vec![1, 2, 3]);
    let held = writer.clone();
    writer.make_mut()[0] = 10;
    assert_eq!(values(&held), [1, 2, 3]);
    assert_eq!(values(&writer), [10, 2, 3]);

    let before = writer.as_ptr();
    writer.make_mut()[1] = 20;
    assert!(std::ptr::eq(before, writer.as_ptr()));
    assert_eq!(values(&writer), [10, 20, 3]);
}

#[test]
fn a_sole_handle_reclaims_the_slots_it_truncated() {
    let mut writer = AppendVec::with_capacity(8);
    writer.extend([1, 2, 3]);
    let before = writer.as_ptr();
    writer.truncate(1);
    writer.push(9);
    assert_eq!(values(&writer), [1, 9]);
    assert!(std::ptr::eq(before, writer.as_ptr()));
}

#[test]
fn every_value_is_dropped_exactly_once() {
    let live = Arc::new(AtomicUsize::new(0));
    {
        let mut writer = AppendVec::new();
        let mut held = Vec::new();
        for value in 0..64 {
            writer.push(Tracked::new(value, &live));
            if value % 5 == 0 {
                held.push(writer.clone());
            }
        }
        let mut fork = held[3].clone();
        fork.push(Tracked::new(1000, &live));
        writer.replace_from(60, [Tracked::new(600, &live)]);
        let mut truncated = writer.clone();
        truncated.truncate(10);
        truncated.make_mut()[0] = Tracked::new(7, &live);
        assert_eq!(writer.len(), 61);
        assert_eq!(writer[60].value, 600);
        assert_eq!(fork.last().map(|tracked| tracked.value), Some(1000));
    }
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_appenders_from_one_snapshot_each_see_only_their_own_values() {
    let mut base = AppendVec::with_capacity(4096);
    base.extend(0..16usize);
    let threads = (0..8usize)
        .map(|thread| {
            let mut handle = base.clone();
            std::thread::spawn(move || {
                for step in 0..200 {
                    handle.push(thread * 1000 + step);
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
    writer.extend(0..8usize);
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
        writer.push(value);
        if value % 3 == 0 {
            sender.send(writer.clone()).expect("reader alive");
        }
    }
    drop(sender);
    assert!(reader.join().expect("reader thread") > 0);
}

#[test]
fn serializes_as_a_plain_sequence() {
    let vec = AppendVec::from(vec![1, 2, 3]);
    let json = serde_json::to_string(&vec).expect("serialize");
    assert_eq!(json, "[1,2,3]");
    let back: AppendVec<i32> = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, vec);
}
