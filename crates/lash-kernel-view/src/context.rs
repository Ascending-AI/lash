//! Sidecar facts for one retained linked document. Nothing here enters identity.
use lash_kernel_doc::{Document, FunctionDefinition};
use lash_kernel_labels::MemberDisplay;
use lash_kernel_package::{LinkRefusal, LinkedContent};
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::CallableId, lash_kernel_value::Fill, lash_kernel_value::ImportSlot,
    lash_kernel_value::LinkId,
};

/// The immutable Link graph and its separately retained display facts.
/// The caller projects labels only from the publications retained by this execution.
#[derive(Clone, Debug)]
pub struct ViewContext {
    pub(crate) reference: lash_kernel_doc::WorkflowReference,
    helpers: std::sync::Arc<lash_kernel_doc::BuildHelpers>,
    content: LinkedContent,
    /// Closure derived once from each immutable Link's document manifest.
    admitted_helpers: BTreeMap<LinkId, BTreeSet<lash_kernel_value::FunctionId>>,
    timers: std::collections::BTreeSet<(lash_kernel_value::PackageId, lash_kernel_value::Segment)>,
    pub(crate) labels: BTreeMap<CallableId, MemberDisplay>,
}
impl ViewContext {
    pub fn new(
        reference: lash_kernel_doc::WorkflowReference,
        content: LinkedContent,
        labels: BTreeMap<CallableId, MemberDisplay>,
        helpers: std::sync::Arc<lash_kernel_doc::BuildHelpers>,
    ) -> Result<Self, LinkRefusal> {
        content.link(reference.link())?;
        let mut admitted_helpers = BTreeMap::new();
        for (id, link) in &content.links {
            content.check_link(link)?;
            admitted_helpers.insert(
                *id,
                helpers.admitted(&content.document(link.document)?.library),
            );
        }
        Ok(Self {
            reference,
            helpers,
            content,
            admitted_helpers,
            labels,
            timers: Default::default(),
        })
    }
    /// Install build-owned display facts for exact helpers in each Link's
    /// admitted library reach. Labels confer no code or operation authority.
    pub fn add_helper_labels(
        &mut self,
        labels: &BTreeMap<lash_kernel_value::FunctionId, String>,
    ) -> Result<(), lash_kernel_labels::LabelError> {
        for (function, json) in labels {
            if !self.helpers.contains(function) {
                return Err(lash_kernel_labels::LabelError::at(
                    "/definitions",
                    &function.to_string(),
                    "helper is not installed",
                ));
            }
            let label = lash_kernel_labels::read_json(&format!(
                "{{\"definitions\":{{\"helper\":{json}}}}}"
            ))?
            .definitions
            .remove("helper")
            .ok_or_else(|| {
                lash_kernel_labels::LabelError::at(
                    "/definitions",
                    "helper",
                    "missing helper display",
                )
            })?;
            for link in self.content.links.keys() {
                if self.admits_helper(&CallableId {
                    link: *link,
                    function: *function,
                }) {
                    self.labels.insert(
                        CallableId {
                            link: *link,
                            function: *function,
                        },
                        label.clone(),
                    );
                }
            }
        }
        Ok(())
    }

    fn admits_helper(&self, callable: &CallableId) -> bool {
        self.admitted_helpers
            .get(&callable.link)
            .is_some_and(|functions| functions.contains(&callable.function))
    }

    pub fn reference(&self) -> &lash_kernel_doc::WorkflowReference {
        &self.reference
    }
    pub(crate) fn document(&self, link: LinkId) -> Option<&Document> {
        self.content
            .links
            .get(&link)
            .and_then(|link| self.content.documents.get(&link.document))
            .map(std::sync::Arc::as_ref)
    }
    pub(crate) fn resolve(&self, link: LinkId, slot: ImportSlot) -> Option<CallableId> {
        let Fill::Export { link, export } = self.content.links.get(&link)?.fills.get(&slot)? else {
            return None;
        };
        Some(CallableId {
            link: *link,
            function: self.content.function_export(*link, export).ok()?.function,
        })
    }
    pub(crate) fn display(&self, callable: &CallableId) -> Option<&MemberDisplay> {
        if let Some(display) = self.labels.get(callable) {
            return Some(display);
        }
        let definition = self.function(callable)?;
        self.labels.iter().find_map(|(other, display)| {
            if other.function != callable.function {
                return None;
            }
            let same_bindings = definition.body().is_none_or(|body| {
                body.imports.iter().all(|(slot, import)| {
                    let key = if self
                        .document(callable.link)
                        .is_some_and(|document| document.functions.contains_key(&callable.function))
                    {
                        (*slot).into()
                    } else {
                        lash_kernel_value::OperationSlot::Helper(
                            lash_kernel_value::HelperOperationId::of_import(import),
                        )
                    };
                    self.content
                        .links
                        .get(&callable.link)
                        .and_then(|link| link.operation_fill(key))
                        == self
                            .content
                            .links
                            .get(&other.link)
                            .and_then(|link| link.operation_fill(key))
                })
            });
            same_bindings.then_some(display)
        })
    }
    pub(crate) fn header(
        &self,
        callable: &CallableId,
    ) -> Option<std::sync::Arc<lash_kernel_doc::FunctionHeader>> {
        let document = self.document(callable.link)?;
        document
            .functions
            .get(&callable.function)
            .map(|definition| std::sync::Arc::new(lash_kernel_doc::FunctionHeader::of(definition)))
            .or_else(|| {
                self.admits_helper(callable)
                    .then(|| {
                        self.helpers
                            .registered(&callable.function)
                            .map(|registered| std::sync::Arc::clone(registered.header()))
                    })
                    .flatten()
            })
    }
    pub(crate) fn function(&self, callable: &CallableId) -> Option<&FunctionDefinition> {
        let document = self.document(callable.link)?;
        document.functions.get(&callable.function).or_else(|| {
            self.admits_helper(callable)
                .then(|| {
                    self.helpers
                        .registered(&callable.function)
                        .map(|registered| &**registered.definition())
                })
                .flatten()
        })
    }

    pub(crate) fn function_graph(&self, callable: &CallableId) -> Option<lash_kernel_check::Graph> {
        let document = self.document(callable.link)?;
        let definition = self.function(callable)?;
        let local = lash_kernel_doc::DocumentCatalog::new(document, self.helpers.as_ref());
        let catalog: &dyn lash_kernel_doc::FunctionCatalog =
            if document.functions.contains_key(&callable.function) {
                &local
            } else {
                self.helpers.as_ref()
            };
        lash_kernel_check::derive_definition(definition, catalog).ok()
    }
}
impl ViewContext {
    /// Join one independently retained sidecar by its immutable Package id.
    pub fn add_labels(
        &mut self,
        id: lash_kernel_value::PackageId,
        json: &str,
    ) -> Result<(), lash_kernel_labels::LabelError> {
        use lash_kernel_package::MemberKind;
        use {lash_kernel_value::ExportName, lash_kernel_value::Segment};
        let missing =
            || lash_kernel_labels::LabelError::at("", "package", "sidecar Package is not retained");
        let package = self.content.packages.get(&id).ok_or_else(missing)?;
        let labels = lash_kernel_package::read_labels(&self.content, package, json)?;
        for (name, display) in labels.members {
            let mut parts = name.split('.');
            let segment = Segment::new(parts.next().unwrap_or_default()).map_err(|_| missing())?;
            let Some(lash_kernel_package::Member {
                kind: MemberKind::Export { link, export },
                ..
            }) = package.members.get(&segment)
            else {
                continue;
            };
            let export = ExportName {
                name: export.name.clone(),
                member: parts
                    .next()
                    .map(Segment::new)
                    .transpose()
                    .map_err(|_| missing())?
                    .or_else(|| export.member.clone()),
            };
            if let Ok(function) = self.content.function_export(*link, &export) {
                self.labels.insert(
                    CallableId {
                        link: *link,
                        function: function.function,
                    },
                    display,
                );
            }
        }
        for (name, display) in labels.definitions {
            for member in package.members.values() {
                if let MemberKind::Export { link, .. } = &member.kind
                    && let Some(document) = self.document(*link)
                {
                    let ids = document
                        .functions
                        .iter()
                        .filter(|(_, definition)| definition.name.as_str() == name)
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>();
                    for function in ids {
                        self.labels.insert(
                            CallableId {
                                link: *link,
                                function,
                            },
                            display.clone(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

impl ViewContext {
    /// The runtime supplies its well-known timer symbols as neutral immutable identities.
    pub fn with_timer_operations(
        mut self,
        timers: impl IntoIterator<Item = (lash_kernel_value::PackageId, lash_kernel_value::Segment)>,
    ) -> Self {
        self.timers.extend(timers);
        self
    }
    pub(crate) fn operation_use(
        &self,
        unit: &lash_kernel_value::sites::Unit,
        slot: ImportSlot,
    ) -> Option<lash_kernel_value::OperationUse> {
        let link = self.reference.link();
        let document = self.document(link)?;
        let slot = match unit {
            lash_kernel_value::sites::Unit::Library(function)
                if !document.functions.contains_key(function) =>
            {
                let import = self.helpers.registered(function)?.imports().get(&slot)?;
                lash_kernel_value::OperationSlot::Helper(
                    lash_kernel_value::HelperOperationId::of_import(import),
                )
            }
            _ => slot.into(),
        };
        Some(lash_kernel_value::OperationUse { link, slot })
    }
    pub(crate) fn operation_import(
        &self,
        use_: &lash_kernel_value::OperationUse,
    ) -> Option<&lash_kernel_value::Import> {
        let link = self.content.links.get(&use_.link)?;
        link.operation_import(&self.document(use_.link)?.imports, use_.slot)
    }
    pub(crate) fn is_timer(&self, use_: lash_kernel_value::OperationUse) -> bool {
        let Some(link) = self.content.links.get(&use_.link) else {
            return false;
        };
        let Some(document) = self.document(use_.link) else {
            return false;
        };
        let Some(import) = link.operation_import(&document.imports, use_.slot) else {
            return false;
        };
        let Some(lash_kernel_value::Fill::Operation(required)) = link.operation_fill(use_.slot)
        else {
            return false;
        };
        self.content.packages.iter().any(|(id, package)| {
            self.timers.contains(&(*id, import.name.member.clone())) && package.path == import.name.package
                && package.members.get(&import.name.member).is_some_and(|member| matches!(&member.kind, lash_kernel_value::MemberKind::Operation(offer) if offer.requirement == *required))
        })
    }
    pub(crate) fn only_timer(
        &self,
        callable: CallableId,
    ) -> Option<lash_kernel_value::OperationUse> {
        let reach = self
            .content
            .inspect(
                callable.link,
                &lash_kernel_package::ReachSelection::Callable(callable.function),
                self.helpers.as_ref(),
            )
            .ok()?;
        let mut uses = reach.operations.keys();
        let use_ = *uses.next()?;
        (uses.next().is_none() && self.is_timer(use_)).then_some(use_)
    }
}

impl ViewContext {
    pub(crate) fn reached(
        &self,
        callable: CallableId,
    ) -> std::collections::BTreeSet<lash_kernel_value::OperationUse> {
        self.content
            .inspect(
                callable.link,
                &lash_kernel_package::ReachSelection::Callable(callable.function),
                self.helpers.as_ref(),
            )
            .map(|reach| reach.operations.into_keys().collect())
            .unwrap_or_default()
    }
}

/// The duration conversion implemented by an exact build-owned helper.
/// Both conversions round down to whole milliseconds and saturate at `i64::MAX`
/// milliseconds. They confer display facts only, never execution authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DurationConversion {
    /// Finite, nonnegative milliseconds, as accepted by TypeScript sleep.
    Milliseconds,
    /// Python sleep's seconds: nonpositive numbers yield a zero duration;
    /// positive numbers must be finite.
    Seconds,
}

impl ViewContext {
    pub(crate) fn projection_role(&self, callable: CallableId) -> lash_kernel_doc::ProjectionRole {
        use lash_kernel_doc::FunctionCatalog as _;
        if !self.admits_helper(&callable) {
            return Default::default();
        }
        self.helpers
            .header(&callable.function)
            .map_or(Default::default(), |header| header.projection)
    }

    fn duration_conversion(&self, callable: CallableId) -> Option<DurationConversion> {
        match self.projection_role(callable) {
            lash_kernel_doc::ProjectionRole::DurationMilliseconds => {
                Some(DurationConversion::Milliseconds)
            }
            lash_kernel_doc::ProjectionRole::TimerSeconds => Some(DurationConversion::Seconds),
            _ => None,
        }
    }

    pub(crate) fn static_duration(
        &self,
        callee: Option<&crate::CalleeView>,
        args: &[crate::ArgumentView],
    ) -> Option<std::time::Duration> {
        use lash_kernel_doc::{ActKind, Callee, Expr, Literal};
        let [argument] = args else { return None };
        let conversion = match callee? {
            crate::CalleeView::Library { id, link, .. } => {
                self.duration_conversion(CallableId {
                    link: *link,
                    function: *id,
                })?
            }
            crate::CalleeView::Effect(_) => {
                // The timer port itself consumes integer milliseconds. A dialect
                // conversion is a separate, pinned call in its argument.
                let callable = |callee: &Callee| match callee {
                    Callee::Local(function) => Some(CallableId {
                        link: self.reference.link(),
                        function: *function,
                    }),
                    Callee::Import(slot) => self.resolve(self.reference.link(), *slot),
                    Callee::Value(_) => None,
                };
                let (function, operands) = match &argument.expression {
                    Expr::Act(act) => {
                        let ActKind::Call(callee) = &act.kind else {
                            return None;
                        };
                        (callable(callee)?, &act.args)
                    }
                    Expr::Call { function, args } => (
                        CallableId {
                            link: self.reference.link(),
                            function: *function,
                        },
                        args,
                    ),
                    Expr::Literal(Literal::Int(milliseconds)) => {
                        return milliseconds
                            .to_string()
                            .parse::<u64>()
                            .ok()
                            .map(std::time::Duration::from_millis);
                    }
                    _ => return None,
                };
                let [Expr::Literal(literal)] = operands.as_slice() else {
                    return None;
                };
                return self.duration_conversion(function)?.literal(literal);
            }
            _ => return None,
        };
        let Expr::Literal(literal) = &argument.expression else {
            return None;
        };
        conversion.literal(literal)
    }
}

impl DurationConversion {
    fn literal(self, literal: &lash_kernel_doc::Literal) -> Option<std::time::Duration> {
        use lash_kernel_doc::Literal;
        let number = match literal {
            Literal::Int(value) => value.to_string().parse::<f64>().ok()?,
            Literal::Float(value) => value.get(),
            _ => return None,
        };
        // These are the pinned helpers' numeric conversions, not an expression
        // evaluator. Variables and arithmetic remain unknown.
        let milliseconds = match self {
            Self::Milliseconds if number.is_finite() && number >= 0.0 => number,
            Self::Seconds if number <= 0.0 => 0.0,
            Self::Seconds if matches!(literal, Literal::Int(_)) || number.is_finite() => {
                if number > 9_223_372_036_854_775.0 {
                    i64::MAX as f64
                } else {
                    number * 1000.0
                }
            }
            _ => return None,
        };
        Some(std::time::Duration::from_millis(
            milliseconds.floor().min(i64::MAX as f64) as i64 as u64,
        ))
    }
}
