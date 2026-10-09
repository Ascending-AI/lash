use lash::persistence::LeaseOwnerIdentity;

fn main() {
    let _ = LeaseOwnerIdentity::opaque("boundary", "writer-0");
}
