//! The run root (cutover contract §6): what a sealed run names of its
//! object graph.

use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

use super::directory::DirectoryRoot;
use super::ids::HighWater;
use super::objects::RunValue;

/// A sealed object graph: its directory, the ids its lineage has minted
/// and the values the run holds outside any object.
#[derive(Clone, Debug, PartialEq)]
pub struct HeapRoot {
    pub high_water: HighWater,
    pub directory: DirectoryRoot,
    pub roots: Vec<RunValue>,
}

impl TokenCodec for HeapRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.high_water.encode(out);
        self.directory.encode(out);
        self.roots.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let root = Self {
            high_water: HighWater::decode(input)?,
            directory: DirectoryRoot::decode(input)?,
            roots: Vec::decode(input)?,
        };
        if let Some(id) = root
            .roots
            .iter()
            .filter_map(RunValue::id)
            .find(|id| !root.high_water.minted(*id))
        {
            return Err(TokenError::Invalid(format!(
                "a root names object {}, past the minted ids",
                id.0
            )));
        }
        Ok(root)
    }
}
