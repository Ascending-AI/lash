//! Read-only registries over a parent's validated, identity-sorted image.
use super::*;
use crate::FrameArray;

/// A build's native constructor. It receives only other pure library natives.
pub type NativeCtor = fn(&FunctionRegistry) -> Arc<dyn NativeFunction>;

pub(super) struct RegistryFrame {
    set: Arc<ReleasedSet>,
    entries: FrameArray<OnceLock<RegisteredFunction>>,
    natives: FrameArray<Option<Binding>>,
    plans: FrameArray<Option<&'static HelperPlan>>,
    buckets: FrameArray<u32>,
}
#[derive(Clone)]
enum Binding {
    Constructor(NativeCtor),
    Native(Arc<dyn NativeFunction>),
    /// The native the function at this position holds: a redeclaration's
    /// root.
    Root(usize),
}
impl fmt::Debug for RegistryFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryFrame")
            .field("functions", &self.set.image.len())
            .finish()
    }
}
impl RegistryFrame {
    fn ensure_unbound(&self) -> Result<(), RegistryError> {
        if self.entries.iter().any(|entry| entry.get().is_some()) {
            return Err(RegistryError::Frame {
                message: "bind before reading frame registrations".into(),
            });
        }
        Ok(())
    }
    pub(super) fn image(&self) -> ReleasedFunctions {
        self.set.image
    }
    pub(super) fn range(&self, function: &FunctionId) -> (usize, usize) {
        if self.buckets.is_empty() {
            (0, self.image().len())
        } else {
            let at = bucket(function);
            (self.buckets[at] as usize, self.buckets[at + 1] as usize)
        }
    }
    fn entry<'a>(&'a self, registry: &FunctionRegistry, at: usize) -> &'a RegisteredFunction {
        self.entries[at].get_or_init(|| {
            let mut registered = RegisteredFunction::from_image(&self.set, at);
            registered.native = self.natives[at].as_ref().and_then(|binding| match binding {
                Binding::Constructor(make) => Some(make(registry)),
                Binding::Native(native) => Some(native.clone()),
                Binding::Root(root) => self.entry(registry, *root).native.clone(),
            });
            registered.plan = self.plans[at];
            registered
        })
    }
}

impl FunctionRegistry {
    /// A lazy view of the parent's trusted frame. No record is merged,
    /// validated, decoded or instantiated here; first use owns each cache.
    pub fn over_frame(bytes: &'static [u8]) -> Result<Self, RegistryError> {
        let image = ReleasedFunctions::over_frame(bytes).map_err(|error| RegistryError::Frame {
            message: error.to_string(),
        })?;
        let slots = |error: std::io::Error| RegistryError::Frame {
            message: error.to_string(),
        };
        let frame = RegistryFrame {
            set: Arc::new(ReleasedSet {
                image,
                facts: FrameArray::mapped(image.len(), |_| OnceLock::new()).map_err(slots)?,
            }),
            entries: FrameArray::mapped(image.len(), |_| OnceLock::new()).map_err(slots)?,
            natives: FrameArray::mapped(image.len(), |_| None).map_err(slots)?,
            plans: FrameArray::mapped(image.len(), |_| None).map_err(slots)?,
            buckets: FrameArray::default(),
        };
        Ok(Self {
            frame: Some(Arc::new(frame)),
            declarations: true,
            ..Self::default()
        })
    }

    /// Position indexes projected once from installed function facts:
    /// native positions, the body-plan domain, each native's root position
    /// and identity bucket boundaries. `roots` names the function each
    /// redeclared one was redeclared from as written: its native is the
    /// root's. Every other native is its own root.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Frame`] when a root is not registered here.
    pub fn frame_positions(
        &self,
        roots: &BTreeMap<FunctionId, FunctionId>,
    ) -> Result<Vec<u8>, RegistryError> {
        let mut natives = Vec::new();
        let mut plans = Vec::new();
        let mut rooted = Vec::new();
        for (at, (function, registered)) in self.iter().enumerate() {
            if registered.states_native() {
                natives.push(at as u32);
                let root = match roots.get(function) {
                    Some(root) => self.position(root).ok_or_else(|| RegistryError::Frame {
                        message: format!("the root {root} of {function} is not registered"),
                    })?,
                    None => at,
                };
                rooted.push(root as u32);
            }
            if registered.has_body() {
                plans.push(at as u32);
            }
        }
        let mut bytes = Vec::with_capacity(8 + 4 * (2 * natives.len() + plans.len()));
        bytes.extend((natives.len() as u32).to_le_bytes());
        bytes.extend((plans.len() as u32).to_le_bytes());
        for position in natives.into_iter().chain(plans).chain(rooted) {
            bytes.extend(position.to_le_bytes());
        }
        let generated;
        let buckets = if let Some(buckets) = &self.buckets {
            buckets.as_ref()
        } else {
            generated = identity_buckets(self.iter().map(|(function, _)| *function));
            generated.as_ref()
        };
        for start in buckets {
            bytes.extend(start.to_le_bytes());
        }
        Ok(bytes)
    }

    /// Binds a build's sorted constructor tables to frame positions, without
    /// constructing any native or reading a definition. Missing Native-only
    /// functions are refused by the same merge that binds optional Both ones.
    pub fn bind_native_tables(
        &mut self,
        positions: &[u8],
        tables: &[&'static [(FunctionId, NativeCtor)]],
        supplied: &BTreeMap<FunctionId, Arc<dyn NativeFunction>>,
    ) -> Result<(), RegistryError> {
        let frame = Arc::get_mut(self.frame.as_mut().ok_or_else(|| RegistryError::Frame {
            message: "native tables require a frame registry".into(),
        })?)
        .ok_or_else(|| RegistryError::Frame {
            message: "bind before sharing the frame".into(),
        })?;
        frame.ensure_unbound()?;
        // The parent already indexed these identities. Copy its ordered
        // bucket boundaries into a private mapping without walking records.
        let count = (1 << BUCKET_BITS) + 1;
        let buckets = &positions[positions.len() - count * 4..];
        frame.buckets = FrameArray::mapped(count, |at| {
            u32::from_le_bytes(
                buckets[at * 4..at * 4 + 4]
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("bucket boundary")),
            )
        })
        .map_err(|error| RegistryError::Frame {
            message: error.to_string(),
        })?;
        let image = frame.set.image;
        for table in tables {
            let mut candidates = positions_of(positions, false).peekable();
            for (function, constructor) in *table {
                while candidates
                    .peek()
                    .is_some_and(|at| image.id(*at) < *function)
                {
                    candidates.next();
                }
                if let Some(at) = candidates.peek().copied()
                    && image.id(at) == *function
                {
                    if frame.natives[at].is_some() {
                        return Err(RegistryError::AlreadyImplemented {
                            name: frame_name(image, at),
                            function: *function,
                        });
                    }
                    frame.natives[at] = Some(Binding::Constructor(*constructor));
                }
            }
        }
        for (function, native) in supplied {
            let at = {
                let mut lo = 0;
                let mut hi = image.len();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if image.id(mid) < *function {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                if lo < image.len() && image.id(lo) == *function {
                    Ok(lo)
                } else {
                    Err(lo)
                }
            }
            .map_err(|_| RegistryError::FrameNativeUnused {
                function: *function,
            })?;
            if !image.states_native(at) {
                return Err(RegistryError::FrameNativeUnused {
                    function: *function,
                });
            }
            if frame.natives[at].is_some() {
                return Err(RegistryError::AlreadyImplemented {
                    name: frame_name(image, at),
                    function: *function,
                });
            }
            frame.natives[at] = Some(Binding::Native(native.clone()));
        }
        // A root is refused before any function redeclared from it.
        let missing = |natives: &FrameArray<Option<Binding>>, at: usize| {
            (!image.has_body(at) && natives[at].is_none()).then(|| {
                RegistryError::FrameNativeMissing {
                    function: image.id(at),
                    name: frame_name(image, at),
                }
            })
        };
        for (at, root) in positions_of(positions, false).zip(roots_of(positions)) {
            if at == root
                && let Some(refusal) = missing(&frame.natives, at)
            {
                return Err(refusal);
            }
        }
        for (at, root) in positions_of(positions, false).zip(roots_of(positions)) {
            if at != root && frame.natives[at].is_none() && frame.natives[root].is_some() {
                frame.natives[at] = Some(Binding::Root(root));
            }
            if let Some(refusal) = missing(&frame.natives, at) {
                return Err(refusal);
            }
        }
        self.declarations = false;
        Ok(())
    }

    /// Installs a build's validated plans by a merge with frame positions.
    /// Every offered plan must name a body in this frame; a stale generated
    /// identity is a build defect, refused before the registry is shared.
    ///
    /// # Errors
    ///
    /// [`RegistryError::FramePlanMissing`] for a plan with no matching body,
    /// and [`RegistryError::Frame`] unless the frame is still unbound.
    pub fn bind_frame_plans(
        &mut self,
        positions: &[u8],
        plans: &[&'static HelperPlan],
    ) -> Result<(), RegistryError> {
        let frame = Arc::get_mut(self.frame.as_mut().ok_or_else(|| RegistryError::Frame {
            message: "plan positions require a frame registry".into(),
        })?)
        .ok_or_else(|| RegistryError::Frame {
            message: "bind before sharing the frame".into(),
        })?;
        frame.ensure_unbound()?;
        let image = frame.set.image;
        let mut candidates = positions_of(positions, true).peekable();
        for plan in plans {
            while candidates
                .peek()
                .is_some_and(|at| image.id(*at) < plan.function)
            {
                candidates.next();
            }
            let at = candidates
                .peek()
                .copied()
                .filter(|at| image.id(*at) == plan.function)
                .ok_or_else(|| RegistryError::FramePlanMissing {
                    function: plan.function,
                    name: FunctionName::written(plan.name),
                })?;
            frame.plans[at].get_or_insert(*plan);
        }
        Ok(())
    }

    pub fn plan_at(&self, at: usize) -> Option<&'static HelperPlan> {
        match &self.frame {
            Some(frame) => frame.plans.get(at).copied().flatten(),
            None => self.functions.get(at).and_then(RegisteredFunction::plan),
        }
    }

    /// The frame's identity at `at`, without materializing its registration.
    pub fn function_at(&self, at: usize) -> Option<FunctionId> {
        match &self.frame {
            Some(frame) => (at < frame.image().len()).then(|| frame.image().id(at)),
            None => self.functions.get(at).map(|registered| registered.function),
        }
    }

    pub(super) fn at(&self, at: usize) -> Option<&RegisteredFunction> {
        match &self.frame {
            Some(frame) => (at < frame.image().len()).then(|| frame.entry(self, at)),
            None => self.functions.get(at),
        }
    }

    /// Editing a frozen registry first gives that editor its own registrations.
    pub(super) fn materialize(&mut self) {
        if self.frame.is_none() {
            return;
        }
        let functions: Vec<_> = (0..self.len())
            .filter_map(|at| self.at(at).cloned())
            .collect();
        let frame = self
            .frame
            .as_ref()
            .unwrap_or_else(|| unreachable!("frozen registry"));
        let image = frame.image();
        let buckets = (!frame.buckets.is_empty()).then(|| frame.buckets.iter().copied().collect());
        let names = (0..image.len())
            .map(|rank| {
                let at = image.name_order(rank);
                Named {
                    name: NameRef::Released(image.name(at)),
                    function: image.id(at),
                }
            })
            .collect();
        self.functions = Arc::new(functions);
        self.names = Arc::new(names);
        self.frame = None;
        self.buckets = buckets;
        if self.buckets.is_none() {
            self.index_buckets();
        }
    }
}

fn frame_name(image: ReleasedFunctions, at: usize) -> FunctionName {
    FunctionName::new(image.name(at)).unwrap_or_else(|_| unreachable!("validated frame name"))
}

/// Each native position's root, in the order of the native positions.
fn roots_of(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let word = |at| {
        u32::from_le_bytes(
            bytes[at..at + 4]
                .try_into()
                .unwrap_or_else(|_| unreachable!("position word")),
        ) as usize
    };
    let natives = word(0);
    let start = 8 + (natives + word(4)) * 4;
    bytes[start..start + natives * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word) as usize)
}

/// Both lists are in identity order, because their positions come from the
/// parent's sorted registry. They are trusted facts, not guest input.
fn positions_of(bytes: &[u8], plans: bool) -> impl Iterator<Item = usize> + '_ {
    let word = |at| {
        u32::from_le_bytes(
            bytes[at..at + 4]
                .try_into()
                .unwrap_or_else(|_| unreachable!("position word")),
        ) as usize
    };
    let natives = word(0);
    let (start, count) = if plans {
        (8 + natives * 4, word(4))
    } else {
        (8, natives)
    };
    bytes[start..start + count * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word) as usize)
}
