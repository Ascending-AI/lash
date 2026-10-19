//! One identity shared by the package law and its native-enabled executable.
pub const BOTH: &str =
    "function pool_fixture.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\nbody { return x }\n";
