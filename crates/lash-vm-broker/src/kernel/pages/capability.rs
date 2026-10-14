//! Page capabilities: the parent's transport authority to read one page
//! (`D-GRANT`).
//!
//! A capability binds a page's digest to its authority (the activation's
//! root lease or stage lease) and the activation's epoch under a keyed
//! blake3 MAC. The key never leaves the parent, so a worker can neither
//! forge a capability for a digest it guessed nor carry one into another
//! activation. Reading a page yields capabilities for exactly the children
//! its envelope declares, so the parent keeps no set of visited pages: the
//! authority of a read is the MAC on its request.

use std::hash::{BuildHasher, Hasher};

use lash_core_store::checkpoint_graph::NodeId;
use lash_vm_protocol::page::{PageAuthority, PageCapability};

/// The parent's MAC key for the capabilities of its activations.
#[derive(Clone)]
pub struct CapabilityKey([u8; 32]);

impl std::fmt::Debug for CapabilityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapabilityKey(..)")
    }
}

impl CapabilityKey {
    /// A key the host supplies.
    #[must_use]
    pub const fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    /// A key drawn from the process's random hasher seeds, which the
    /// standard library takes from the operating system's entropy: 128
    /// bits a worker cannot predict, stretched to the MAC's key size.
    #[must_use]
    pub fn fresh() -> Self {
        let mut seed = blake3::Hasher::new_derive_key("lash page capability key v1");
        for lane in 0_u64..4 {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u64(lane);
            seed.update(&hasher.finish().to_le_bytes());
        }
        Self(*seed.finalize().as_bytes())
    }

    /// The capability for `page` under `authority` in activation `epoch`.
    #[must_use]
    pub fn issue(&self, page: NodeId, authority: PageAuthority, epoch: u64) -> PageCapability {
        PageCapability {
            page: page.0,
            authority,
            epoch,
            mac: self.mac(&page.0, authority, epoch),
        }
    }

    /// Whether `capability` is one this key issued: its MAC checks against
    /// its own page, authority and epoch. The caller checks the authority
    /// and epoch are the activation's.
    #[must_use]
    pub fn verify(&self, capability: &PageCapability) -> bool {
        let expected = self.mac(&capability.page, capability.authority, capability.epoch);
        // Constant time: blake3's hash equality is.
        blake3::Hash::from(expected) == blake3::Hash::from(capability.mac)
    }

    fn mac(&self, page: &[u8; 32], authority: PageAuthority, epoch: u64) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        hasher.update(page);
        let (tag, id) = match authority {
            PageAuthority::Root(id) => (0_u8, id),
            PageAuthority::Stage(id) => (1_u8, id),
        };
        hasher.update(&[tag]);
        hasher.update(&id.to_le_bytes());
        hasher.update(&epoch.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}
