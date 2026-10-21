//! The pool's immutable native library image. Linked documents and slots travel per execution.
use crate::PoolError;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_vm::PreparedLibrary;
use lash_protocol_types::engine_fault::{PayloadKind, WorkerLibraryRefusal};
use lash_vm_protocol::{EncodedPayload, FrameCodec, PoolStarted};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug)]
pub struct WorkerLibrary {
    frame: Arc<[u8]>,
    live: Arc<OnceLock<&'static [u8]>>,
    registry: Arc<FunctionRegistry>,
    roots: std::collections::BTreeSet<lash_vocabulary::names::FunctionId>,
}

impl WorkerLibrary {
    pub fn new(
        functions: &FunctionRegistry,
        roots: impl IntoIterator<Item = lash_vocabulary::names::FunctionId>,
    ) -> Result<Self, WorkerLibraryRefusal> {
        let roots: std::collections::BTreeSet<_> = roots.into_iter().collect();
        let root_bytes = lash_vocabulary::tokens::encode_body(&roots);
        let image = functions
            .to_image_with_census(&|definition| lash_kernel_vm::body_census(definition, functions));
        let index = PreparedLibrary::frame_index(functions);
        let positions = functions
            .frame_positions(&Default::default())
            .map_err(malformed)?;
        let mut frame = Vec::new();
        for part in [&image, &index, &positions, &root_bytes] {
            let length: u32 = part.len().try_into().map_err(malformed)?;
            frame.extend_from_slice(&length.to_le_bytes());
            frame.extend_from_slice(part);
        }
        Ok(Self {
            frame: frame.into(),
            live: Arc::new(OnceLock::new()),
            registry: Arc::new(functions.clone()),
            roots,
        })
    }
    pub fn roots(&self) -> &std::collections::BTreeSet<lash_vocabulary::names::FunctionId> {
        &self.roots
    }
    pub fn registry(&self) -> &FunctionRegistry {
        &self.registry
    }
    pub fn frame(&self) -> Arc<[u8]> {
        self.frame.clone()
    }
    pub fn from_frame(frame: Arc<[u8]>) -> Result<Self, WorkerLibraryRefusal> {
        sections(&frame)?;
        let owner: &'static [u8] = Box::leak(frame.to_vec().into_boxed_slice());
        let [image, _, _, roots] = sections(owner)?;
        let roots = lash_vocabulary::tokens::decode_body(roots).map_err(malformed)?;
        let registry = Arc::new(FunctionRegistry::over_frame(image).map_err(malformed)?);
        Ok(Self {
            frame,
            live: Arc::new(OnceLock::from(owner)),
            registry,
            roots,
        })
    }
    pub fn indexes(&self) -> Result<[&'static [u8]; 4], WorkerLibraryRefusal> {
        let bytes = self
            .live
            .get_or_init(|| Box::leak(self.frame.to_vec().into_boxed_slice()));
        sections(bytes)
    }
}
fn malformed(error: impl std::fmt::Display) -> WorkerLibraryRefusal {
    WorkerLibraryRefusal::Malformed {
        problem: error.to_string(),
    }
}
fn sections(bytes: &[u8]) -> Result<[&[u8]; 4], WorkerLibraryRefusal> {
    let mut parts = [&[][..]; 4];
    let mut at = 0usize;
    for part in &mut parts {
        let word: [u8; 4] = bytes
            .get(at..at + 4)
            .ok_or_else(|| malformed("library section header"))?
            .try_into()
            .map_err(malformed)?;
        at += 4;
        let end = at
            .checked_add(u32::from_le_bytes(word) as usize)
            .ok_or_else(|| malformed("library section overflow"))?;
        *part = bytes
            .get(at..end)
            .ok_or_else(|| malformed("library section is truncated"))?;
        at = end;
    }
    if at != bytes.len() {
        return Err(malformed("library frame has trailing bytes"));
    }
    Ok(parts)
}
pub fn read(frame: Arc<[u8]>) -> Result<WorkerLibrary, PoolError> {
    WorkerLibrary::from_frame(frame).map_err(PoolError::Library)
}
pub fn result(result: Result<(), WorkerLibraryRefusal>) -> Result<PoolStarted, PoolError> {
    Ok(PoolStarted {
        result: EncodedPayload(
            rmp_serde::to_vec_named(&result)
                .map_err(|error| PoolError::payload(PayloadKind::PoolStart, error))?,
        ),
    })
}
pub(crate) fn installed(reply: PoolStarted, codec: &FrameCodec) -> Result<(), PoolError> {
    codec.check_payload(&reply.result.0)?;
    let result: Result<(), WorkerLibraryRefusal> = rmp_serde::from_slice(&reply.result.0)
        .map_err(|error| PoolError::payload(PayloadKind::PoolStart, error))?;
    result.map_err(PoolError::Library)
}
