//! FIG-3802's laws over the guarded surfaces `lash-restate` owns: the
//! stamped value families of its virtual objects, each written through the
//! family's fleet-selected writer and read through the production stamped
//! decoder, which lifts an older stamp through the family's registered
//! upcaster rows.

use lash_core_store::testing::guarded_surfaces::{self as laws, SurfaceProbe};

use lash_core::FleetFormat;

use crate::durable_wait::DURABLE_WAIT_REGISTRY_FORMATS;
use crate::effect_group::{EFFECT_GROUP_PAYLOAD_FAMILY, EFFECT_GROUP_STATE_FORMATS};
use crate::object_state::{FORMAT_FIELD, StampedValue, StoredValueFormats, decode_stamped_bytes};
use crate::session_driver::TURN_OUTCOME_FORMATS;

const OWNER: &str = "lash-restate";

/// One fixed value, as a family's handler stores it.
fn body() -> serde_json::Value {
    serde_json::json!({"name": "alpha", "count": 3, "bytes": [7, 8]})
}

fn write(formats: &StoredValueFormats, fleet: FleetFormat) -> Vec<u8> {
    serde_json::to_vec(&StampedValue {
        format: formats.writer(fleet).format(),
        body: body(),
    })
    .expect("encode a stamped value")
}

fn read(formats: &StoredValueFormats, bytes: &[u8]) -> Result<String, String> {
    decode_stamped_bytes::<serde_json::Value>("state-key", bytes, formats)
        .map(|body| body.to_string())
        .map_err(|error| error.to_string())
}

fn restamp(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("a stamped value");
    value[FORMAT_FIELD] = serde_json::json!(version);
    serde_json::to_vec(&value).expect("encode")
}

/// The probe of one family, whose [`StoredValueFormats`] are `$formats`.
macro_rules! family_probe {
    ($constant:ident, $formats:expr) => {{
        fn write_family(fleet: FleetFormat) -> Vec<u8> {
            write($formats, fleet)
        }
        fn read_family(bytes: &[u8], _fleet: FleetFormat) -> Result<String, String> {
            read($formats, bytes)
        }
        SurfaceProbe {
            constant: stringify!($constant),
            newest: ($formats).newest(),
            write: write_family,
            read: read_family,
            restamp,
        }
    }};
}

fn probes() -> Vec<SurfaceProbe> {
    vec![
        family_probe!(
            EFFECT_GROUP_STATE_FORMAT_VERSION,
            &EFFECT_GROUP_STATE_FORMATS
        ),
        family_probe!(
            EFFECT_GROUP_PAYLOAD_FORMAT_VERSION,
            EFFECT_GROUP_PAYLOAD_FAMILY.formats
        ),
        family_probe!(
            DURABLE_WAIT_REGISTRY_FORMAT_VERSION,
            &DURABLE_WAIT_REGISTRY_FORMATS
        ),
        family_probe!(LASH_TURN_OUTCOME_FORMAT_VERSION, &TURN_OUTCOME_FORMATS),
    ]
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
