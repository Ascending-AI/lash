fn main() {
    let _: () = lash_vocabulary::identity::ProcessId::fixture("fixture");
}

// The Cargo UI graph enables testing through dev-dependencies; this mismatch proves availability.
