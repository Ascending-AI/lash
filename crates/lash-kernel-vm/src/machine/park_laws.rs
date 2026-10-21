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
            "definition \"function child(i: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{  let r = perform 0(i) as Any return r  }}\"\n\nmain {{\n  let hs = []\n  let i = 0\n  while num.lt(i, {W}) {{\n    let h = spawn call child(i)\n    set hs[list.len(hs)] = h\n    set i = num.add(i, 1)\n  }}\n  let n = 0\n  for h in hs {{\n    let r = join h\n    set n = num.add(n, 1)\n  }}\n  return n\n}}"
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
