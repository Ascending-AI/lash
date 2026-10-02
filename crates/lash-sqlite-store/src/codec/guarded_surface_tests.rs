//! FIG-3802's laws over the guarded surface `lash-sqlite-store` owns: the
//! stored blob envelope, executed through the production encoder and decoder.

use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use super::*;
use lash_core_store::store::FleetFormat;

const OWNER: &str = "lash-sqlite-store";

/// Compressible content, so the envelope's codec field is exercised too.
const CONTENT: &[u8] = &[b'x'; 8192];

fn write_envelope(fleet: FleetFormat) -> Vec<u8> {
    encode_artifact_blob(
        &BlobArtifactDescriptor::checkpoint_component(),
        BuiltinBlobProfile::Balanced,
        CONTENT,
        fleet.writer_version(lash_core_execution::surface_format!(
            SQLITE_BLOB_ENVELOPE_VERSION
        )),
    )
    .expect("encode the blob envelope")
}

fn read_envelope(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
    decode_artifact_blob(bytes)
        .map(|content| String::from_utf8_lossy(&content).into_owned())
        .map_err(|error| error.to_string())
}

fn restamp_envelope(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut envelope: StoredBlobEnvelope = rmp_serde::from_slice(bytes).expect("an envelope");
    envelope.version = version;
    encode_msgpack(&envelope, "blob fixture").expect("encode")
}

fn probes() -> Vec<SurfaceProbe> {
    vec![SurfaceProbe {
        constant: "SQLITE_BLOB_ENVELOPE_VERSION",
        newest: SQLITE_BLOB_ENVELOPE_VERSION,
        write: write_envelope,
        read: read_envelope,
        restamp: restamp_envelope,
    }]
}

#[test]
fn every_guarded_surface_decodes_its_supported_range() {
    laws::every_guarded_surface_decodes_its_supported_range(OWNER, &probes());
}

#[test]
fn unknown_version_is_refused_with_zero_mutation() {
    laws::unknown_version_is_refused_with_zero_mutation(OWNER, &probes());
}

#[test]
fn upcast_preserves_immutable_bytes_and_hashes() {
    laws::upcast_preserves_immutable_bytes_and_hashes(OWNER, &probes());
}
