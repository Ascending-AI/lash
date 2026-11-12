use super::{Drive, inlined, int, reference};

/// K-CHG-002 and K-BND-003: a lookup followed by a word helper keeps
/// both logical frames, both completion events and the caller blocks.
#[test]
fn a_lookup_followed_by_words_keeps_each_frame_and_completion() {
    let before = crate::vm::lookup::ran();
    let text = r#"main {
  let m = map{"a": 17}
  let total = 0
  set total = law.word(total, invoke law.lookup(m, "a"))
  print total
  let measured = 0
  set measured = law.word(measured, invoke law.magnitude_lookup(m, "a"))
  print measured
  set m["a"] = 23
  set total = law.word(total, invoke law.lookup(m, "a"))
  print total
  set m["a"] = 0
  set total = law.word(total, invoke law.lookup(m, "a"))
  try { set total = law.word(total, invoke law.strict_lookup(m, "a")) } catch e { print 0 }
  try { set total = law.word(total, invoke law.lookup(m, "missing")) } catch e { print 0 }
  set m["a"] = "wrong kind"
  try { set total = law.word(total, invoke law.lookup(m, "a")) } catch e { print 0 }
  set m["a"] = 1
  let big = 9223372036854775807
  try { set big = law.word(big, invoke law.lookup(m, "a")) } catch e { set big = absent }
  let r = {a: 9}
  set total = law.word(total, invoke law.lookup(r, "a"))
  return [total, big]
}"#;
    let typed = Drive {
        typed: true,
        ..Drive::default()
    };
    let whole = inlined(text, typed);
    assert_eq!(
        whole.printed,
        [int(17), int(17), int(40), int(0), int(0), int(0)]
    );
    assert_eq!(
        whole.end,
        format!(
            "{:?}",
            lash_kernel_doc::Datum::List(vec![int(49), lash_kernel_doc::Datum::Absent])
        )
    );
    assert!(crate::vm::lookup::ran() > before, "no composed lookup ran");
    let (held, get_frame, word_frame) = crate::vm::lookup::frame_sizes().unwrap();
    assert!(
        word_frame > get_frame,
        "frame fixture: getter {get_frame}, word {word_frame}"
    );
    let memory = held + get_frame + (word_frame - get_frame) / 2;
    let how = Drive { memory, ..typed };
    let refused = reference(text, how);
    assert!(
        refused.end.contains("Memory"),
        "target frame budget {memory}: {refused:?}"
    );
    assert_eq!(
        inlined(text, how),
        refused,
        "memory {memory}, getter {get_frame}, word {word_frame}, held {held}"
    );
    let total = whole.charged.unwrap();
    for cold in [false, true] {
        for exact in [false, true] {
            for slice in [u64::MAX, 1, 7] {
                let how = Drive {
                    cold,
                    exact,
                    slice,
                    ..typed
                };
                assert_eq!(inlined(text, how), reference(text, how));
            }
        }
        for limit in 0..=total {
            let how = Drive {
                cold,
                limit,
                ..typed
            };
            assert_eq!(
                inlined(text, how),
                reference(text, how),
                "limit {limit}, cold {cold}"
            );
        }
        for memory in (800..8000).step_by(97) {
            let how = Drive {
                cold,
                memory,
                ..typed
            };
            assert_eq!(
                inlined(text, how),
                reference(text, how),
                "memory {memory}, cold {cold}"
            );
        }
    }
}

pub(super) fn machine(
    text: &str,
    bounds: crate::Bounds,
) -> Result<crate::CoreMachine, crate::StartError> {
    use crate::Machine;
    use crate::laws::embedder;
    use std::sync::Arc;
    let library = embedder::shared_library(true, Some(super::huge_charges), true);
    let helpers = Arc::new(
        lash_kernel_doc::BuildHelpers::new(
            (*library.registry).clone(),
            ["law.lookup", "law.strict_lookup"].map(|name| library.ids[name]),
        )
        .unwrap(),
    );
    let document = Arc::new(embedder::document(
        &format!("numbers by_spelling\n{}{text}", library.header),
        &library.registry,
    ));
    let program = embedder::program(
        document,
        crate::PreparedLibrary::with_helpers(library.registry, helpers),
    );
    crate::CoreMachine::start(
        program,
        bounds,
        embedder::Setup::default().start,
        Box::new(embedder::Source::default()),
    )
}
