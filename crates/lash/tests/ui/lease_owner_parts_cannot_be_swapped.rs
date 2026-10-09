use lash::persistence::{LeaseIncarnationId, LeaseOwnerId, LeaseOwnerIdentity};

fn main() {
    let node = LeaseOwnerId::new("writer-0");
    let boot = LeaseIncarnationId::new("boundary-boot");
    let _ = LeaseOwnerIdentity::opaque(boot, node);
}
