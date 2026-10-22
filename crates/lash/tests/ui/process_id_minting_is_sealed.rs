// Only the registrar mints a process id (ADR 0107): a host or tool can parse
// an id a registrar minted, never forge one from a number.
fn main() {
    let _ = lash::ProcessId::from_minted(0x0192_0000_0000_7000_8000_0000_0000_0001);
    let _ = lash::process::ProcessIdRegistrar::REGISTRAR;
}
