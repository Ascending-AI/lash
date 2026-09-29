use lash::process::{ProcessStartRequest, StartKey};

// A tool body holds only its attempt context: it has no start to submit a key
// to, a request's key is private, and every family but the host's is derived
// under an authority no facade exports (ADR 0107, FIG-4111).
async fn attempt_body(context: &lash::tools::AttemptContext<'_>, request: ProcessStartRequest) {
    let _ = context.processes().start(request.clone());
    let _ = request.clone().with_start_key(Some(StartKey::for_host("model-chosen")));
    let _ = &request.start_key;
    let _ = StartKey::for_keyless_host(lash::process::StartKeyDerivation::LASH_START_PATHS);
}

fn main() {}
