//! Library code compiled once (cutover contract §1): each library body a
//! run reaches is compiled the first time any run of its prepared library
//! reaches it, as a template no Link owns, and every later start
//! instantiates the template into its own [`Program`](super::program::Program)
//! in the Link that reaches it.
//!
//! A template is code with its outside left open: its units, its constant
//! pool and the host bindings it reads are its own, and every other body,
//! native, completion formula and function value it names is named by
//! function. Instantiation lays its units out in the program, resolves
//! what it names in the reaching Link's scope, marks its call and action
//! sites among that Link's occurrence counters and gives its closures and
//! function values that Link. A program built from templates is the one
//! compiling each body in place would build, up to the numbering of its
//! units and constants.
//!
//! Templates are derived from the registry, its native linkage and the
//! build image an open compiles under, and are never saved: a
//! [`Library`] belongs to one prepared library and one build image, so no
//! other registry ever reads it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{BuildHelpers, FunctionRegistry};
use lash_kernel_value::names::{FunctionId, Name};

use super::compile::{Compiler, Unsupported};
use super::program::{Constant, Unit};

/// A unit id in a template naming a body outside it: the low bits index
/// [`Template::bodies`].
pub(crate) const OUTSIDE: u32 = 1 << 31;

/// The templates of one prepared library under one build image, compiled
/// as runs first reach them. A body the core cannot compile is kept as
/// its refusal.
#[derive(Default)]
pub(crate) struct Library {
    templates: Mutex<HashMap<(FunctionId, bool), Compiled>>,
}

/// A body's template, or why the core cannot compile it.
type Compiled = Result<Arc<Template>, Unsupported>;

impl Library {
    /// The template of library function `id`'s body, `covered` or not,
    /// compiled now if no run has reached it.
    pub(crate) fn template(
        &self,
        registry: &FunctionRegistry,
        available: Option<&BuildHelpers>,
        id: FunctionId,
        covered: bool,
    ) -> Result<Arc<Template>, Unsupported> {
        let mut templates = self
            .templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        templates
            .entry((id, covered))
            .or_insert_with(|| Compiler::template(registry, available, id, covered).map(Arc::new))
            .clone()
    }
}

/// One library body compiled with its outside open. Unit 0 is the body;
/// the others are the closures, spawn stubs and refusing units its code
/// made.
pub(crate) struct Template {
    pub units: Vec<Unit>,
    pub constants: Vec<Constant>,
    /// The host bindings its code reads, by its `Global` index.
    pub globals: Vec<Name>,
    /// The bodies it calls, by [`OUTSIDE`] index.
    pub bodies: Vec<(FunctionId, bool)>,
    /// The natives it calls, by its `Callee::Native` index.
    pub natives: Vec<FunctionId>,
    /// The completion formulas its calls bill, by its charge index.
    pub charges: Vec<FunctionId>,
    /// The functions its function literals name: each needs the reaching
    /// Link's trampoline.
    pub functions: Vec<FunctionId>,
    /// The body reads names its own Link binds, which only the root
    /// Link's code may (its free names are host bindings of main).
    pub reads: bool,
}

/// One prepared library's templates, by the build image an open compiles
/// under (`None`: the registry's own). A run opened under an image it
/// was not sealed under is rare, so a few images are kept, the oldest
/// dropped first.
#[derive(Default)]
pub(crate) struct Libraries {
    by_image: Mutex<Vec<(Option<lash_kernel_value::HelperLibraryId>, Arc<Library>)>>,
}

/// The most build images whose templates one prepared library keeps.
const IMAGES: usize = 4;

impl Libraries {
    /// The templates compiled under `available`.
    pub(crate) fn under(&self, available: Option<&BuildHelpers>) -> Arc<Library> {
        let image = available.map(BuildHelpers::identity);
        let mut by_image = self
            .by_image
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = by_image.iter().position(|(held, _)| *held == image) {
            let found = by_image.remove(at);
            let library = Arc::clone(&found.1);
            by_image.push(found);
            return library;
        }
        if by_image.len() == IMAGES {
            by_image.remove(0);
        }
        let library = Arc::new(Library::default());
        by_image.push((image, Arc::clone(&library)));
        library
    }
}
