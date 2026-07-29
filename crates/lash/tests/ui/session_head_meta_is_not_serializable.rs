use lash::persistence::SessionHeadMeta;

fn requires_serialize<T: serde::Serialize>() {}

fn main() {
    requires_serialize::<SessionHeadMeta>();
}
