//! Cold fan-out delivery through the machine interface.

use crate::laws::embedder::{Embedder, ROOMY, Setup};
use crate::{Bounds, Layout, Step};

/// K-TASK-024 / K-MACH-007: a thousand-way fan-out reports its requests,
/// accepts one delivery per cold reopen and completes every child once.
#[test]
fn a_thousand_way_fan_out_delivered_one_at_a_time_reads_linear_wait_pages() {
    const W: u64 = 1_000;
    let bounds = Bounds {
        live_tasks: 1_001,
        requests_per_park: 1_000,
        ..ROOMY
    };
    let mut embedder = Embedder::with(
        &format!(
            "fn child(i) {{ let r = perform echo(i) as Any return r }}
main {{
  let hs = []
  let i = 0
  while num.lt(i, {W}) {{
    let h = spawn call child(i)
    set hs[list.len(hs)] = h
    set i = num.add(i, 1)
  }}
  let n = 0
  for h in hs {{
    let r = join h
    set n = num.add(n, 1)
  }}
  return n
}}"
        ),
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    let step = embedder.run();
    let Step::Parked(park) = step else {
        panic!("every child waits: {step:?}");
    };
    assert_eq!(park.requests.len() as u64, W);
    let mut delivered = 0;
    let end = loop {
        embedder.reopen(Layout::default(), true);
        embedder.deliver_next();
        delivered += 1;
        let step = embedder.run();
        if let Step::Ended(end) = step {
            break end;
        }
        assert!(matches!(step, Step::Parked(_)));
    };
    assert_eq!(
        crate::laws::embedder::result(end),
        crate::laws::embedder::int(W as i64)
    );
    assert_eq!(delivered, W);
}
