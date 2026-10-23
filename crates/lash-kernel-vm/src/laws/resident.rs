//! A durable run executes in memory between its projection points (seams
//! C and G, FIG-6748): a session write keeps its arena value, a collection
//! replaced before any projection saw it takes no id and is never written
//! back, and a refinement under memory pressure prices what the run holds
//! where it is, publishing nothing.

use super::embedder::{Embedder, Setup, int, result};
use crate::perf_witness::Work;
use crate::{Bounds, End};

/// The work a run of `text` does from its start to its end, with its end.
fn counted(text: &str, setup: Setup) -> (Work, End) {
    let mut embedder = Embedder::with(text, setup);
    crate::perf_witness::enable_work();
    let end = embedder.run_to_end(&[]);
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    (work, end)
}

/// A top-level loop that writes two session bindings `n` times.
fn session_loop(n: u64) -> String {
    format!(
        r#"
main {{
  let total = 0
  let i = 0
  while num.lt(i, {n}) {{
    set total = num.add(total, i)
    set i = num.add(i, 1)
  }}
  return total
}}
"#
    )
}

/// Seam C: a session write gives its value no page form. Ten iterations
/// of a top-level loop and two hundred convert as many values to their
/// page forms and give as many ids: those of the projection points only.
#[test]
fn top_level_session_writes_give_no_page_form_between_projection_points() {
    let (few, end) = counted(&session_loop(10), Setup::default());
    assert_eq!(result(end), int(45));
    let (many, end) = counted(&session_loop(200), Setup::default());
    assert_eq!(result(end), int(19_900));
    assert_eq!(
        (few.page_forms, few.heap.identified),
        (many.page_forms, many.heap.identified),
        "session writes converted values to their page forms"
    );
}

/// A top-level loop that replaces the list a session binding holds `n`
/// times.
fn replacing_loop(n: u64) -> String {
    format!(
        r#"
main {{
  let kept = {{value: []}}
  let alias = kept
  let i = 0
  while num.lt(i, {n}) {{
    let previous = kept.value
    set alias.value = [i, i, i]
    set i = num.add(i, 1)
  }}
  return list.len(kept.value)
}}
"#
    )
}

/// Seam C and seam D: a list a session binding held and lost before any
/// projection point never takes an id and is never written back; only
/// the one the end saves is.
#[test]
fn a_replaced_collection_no_projection_saw_is_never_identified_or_written() {
    let (few, end) = counted(&replacing_loop(10), Setup::default());
    assert_eq!(result(end), int(3));
    let (many, end) = counted(&replacing_loop(200), Setup::default());
    assert_eq!(result(end), int(3));
    assert_eq!(
        (few.heap.identified, few.heap.projection_visits),
        (many.heap.identified, many.heap.projection_visits),
        "replaced collections were identified or written back"
    );
}

/// A function that keeps replacing what it holds: under a tight bound its
/// growth needs refinements to fit.
const CHURN: &str = r#"
definition "function make(n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  return [n, n, n, n]\n }"

definition "function work(n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let keep = [0, 1, 2, 3, 4, 5, 6, 7]\n  let i = 0\n  while num.lt(i, n) {\n    set keep[0] = call make(i)\n    set i = num.add(i, 1)\n  }\n  return list.len(keep)\n }"

main {
  let r = call work(3000)
  return r
}
"#;

fn bounded(memory: u64) -> Setup {
    Setup {
        bounds: Bounds {
            memory,
            ..Setup::default().bounds
        },
        ..Setup::default()
    }
}

/// Seam G: a refinement under pressure publishes no root and gives no id.
/// Under the least bound the run fits, it refines, and it publishes and
/// identifies exactly what it does unbounded: at its projection points.
#[test]
fn a_pressure_refinement_publishes_nothing() {
    let (roomy, end) = counted(CHURN, Setup::default());
    assert_eq!(result(end), int(8));
    assert_eq!(roomy.heap.completed_refinements, 0, "the roomy run refined");
    let fits = (10..24)
        .map(|power| 1_u64 << power)
        .find(|memory| matches!(counted(CHURN, bounded(*memory)).1, End::Finished(_)))
        .expect("the run fits some bound");
    // Within a factor of two of the least power of two it fits.
    let tight = (fits / 2..=fits)
        .step_by((fits / 64) as usize)
        .find(|memory| matches!(counted(CHURN, bounded(*memory)).1, End::Finished(_)))
        .expect("the run fits its bound");
    let (pressed, end) = counted(CHURN, bounded(tight));
    assert_eq!(result(end), int(8));
    assert!(
        pressed.heap.completed_refinements > 0,
        "the run under {tight} bytes did not refine"
    );
    assert_eq!(
        (pressed.root_publishes, pressed.heap.identified),
        (roomy.root_publishes, roomy.heap.identified),
        "a refinement under {tight} bytes published or identified"
    );
}
