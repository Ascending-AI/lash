//! Kernel-only library installation. Workers use this same native/plan binding.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::{BuildHelpers, FunctionRegistry, NativeFunction};
use lash_kernel_value::FunctionId;
use lash_kernel_vm::PreparedLibrary;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Registry(#[from] lash_kernel_doc::RegistryError),
    #[error("library installation failed: {0}")]
    Library(String),
}

pub struct Installed {
    pub registry: Arc<FunctionRegistry>,
    pub names: NamedLibrary,
    pub prepared: PreparedLibrary,
}

/// Binds an immutable library image without introducing host transport types.
/// The caller must supply indexes produced for this exact image.
pub fn install(
    image: &'static [u8],
    index: &'static [u8],
    positions: &[u8],
    roots: &BTreeSet<FunctionId>,
    helpers: Arc<BuildHelpers>,
    natives: &BTreeMap<FunctionId, Arc<dyn NativeFunction>>,
) -> Result<Installed, InstallError> {
    let mut registry = FunctionRegistry::over_frame(image)?;
    registry.bind_native_tables(positions, crate::native_tables::native_tables(), natives)?;
    #[cfg(feature = "dialect-typescript")]
    registry.bind_frame_plans(positions, lash_ext_ts_core::PLANS)?;
    #[cfg(feature = "dialect-python")]
    registry.bind_frame_plans(positions, lash_ext_py_core::PLANS)?;
    let registry = Arc::new(registry);
    let names = NamedLibrary::resolving(&registry, |id| roots.contains(id))
        .map_err(|e| InstallError::Library(e.to_string()))?;
    let prepared =
        PreparedLibrary::over_frame(registry.clone(), lash_kernel_vm::Charge::Exact, index)
            .map_err(|e| InstallError::Library(e.to_string()))?
            .with_helper_handle(helpers);
    Ok(Installed {
        registry,
        names,
        prepared,
    })
}

/// Process-lifetime immutable code indexes, shared by all standalone engines.
/// This has the same lifetime as the build-owned helper release.
pub(crate) fn standard() -> Result<Installed, InstallError> {
    struct Image {
        image: &'static [u8],
        index: &'static [u8],
        positions: Vec<u8>,
    }
    static IMAGE: OnceLock<Result<Image, String>> = OnceLock::new();
    let release =
        lash_kernel_releases::helper_release().map_err(|e| InstallError::Library(e.to_string()))?;
    let image = IMAGE
        .get_or_init(|| {
            let functions = &release.functions;
            let positions = functions
                .frame_positions(&BTreeMap::new())
                .map_err(|e| e.to_string())?;
            let image = functions.to_image_with_census(&|definition| {
                lash_kernel_vm::body_census(definition, functions)
            });
            let index = PreparedLibrary::frame_index(functions);
            Ok(Image {
                image: Box::leak(image.into_boxed_slice()),
                index: Box::leak(index.into_boxed_slice()),
                positions,
            })
        })
        .as_ref()
        .map_err(|e| InstallError::Library(e.clone()))?;
    install(
        image.image,
        image.index,
        &image.positions,
        &release.roots,
        Arc::clone(&release.helpers),
        &BTreeMap::new(),
    )
}
