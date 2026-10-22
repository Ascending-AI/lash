//! Decorating a presenter: a heading, a tool filter, an appended section.

use std::sync::Arc;

use lash_vocabulary::json_schema::JsonSchema;
use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::tool_contract::ToolManifest;
use serde_json::Value;

use crate::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter,
};

type OmissionRenderer =
    dyn Fn(&lash_protocol_types::AttachmentMaterializationNotice) -> String + Send + Sync;

type ToolFilter = dyn Fn(&ToolPresentationEntry<'_>) -> bool + Send + Sync;

/// A presenter that presents through `inner` and decorates its section: a
/// heading before it, a host section after it, and a filter that leaves
/// tools out of the listing. Its id is its own, so a session records the
/// decorated presenter, not the one it wraps; its config is `inner`'s.
///
/// A tool the filter leaves out is reported omitted, never dropped, and a
/// pinned tool (the discovery tool, a control tool) is always listed. Docs,
/// signatures and discovery text are `inner`'s unchanged.
///
/// ```rust,ignore
/// use lash_tool_presenter_api::{ToolPresenterSlot, WrappedToolPresenter};
/// use lash_tool_presenter_typescript::TypescriptToolPresenter;
///
/// let slot = ToolPresenterSlot::new(
///     WrappedToolPresenter::new("acme.tools.v1", TypescriptToolPresenter)
///         .with_heading("Acme tools. Prefer `search` before `fetch`.")
///         .with_filter(|tool| !tool.call_path.starts_with("internal."))
///         .with_appended("Report every failed call to the user."),
/// );
/// ```
#[derive(Clone)]
pub struct WrappedToolPresenter {
    id: String,
    inner: Arc<dyn ToolPresenter>,
    heading: Option<String>,
    appended: Option<String>,
    filter: Option<Arc<ToolFilter>>,
    omission_renderer: Option<Arc<OmissionRenderer>>,
}

impl WrappedToolPresenter {
    /// `inner`, under the id `id`, with no decoration yet.
    pub fn new(id: impl Into<String>, inner: impl ToolPresenter + 'static) -> Self {
        Self::wrapping(id, Arc::new(inner))
    }

    /// The presenter `inner` already shared, under the id `id`.
    pub fn wrapping(id: impl Into<String>, inner: Arc<dyn ToolPresenter>) -> Self {
        Self {
            id: id.into(),
            inner,
            heading: None,
            appended: None,
            filter: None,
            omission_renderer: None,
        }
    }

    /// Replace the wrapped presenter's request-only attachment omission line.
    #[must_use]
    pub fn with_attachment_omission_renderer(
        mut self,
        render: impl Fn(&lash_protocol_types::AttachmentMaterializationNotice) -> String
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.omission_renderer = Some(Arc::new(render));
        self
    }

    /// Text before the section, separated from it by a blank line.
    #[must_use]
    pub fn with_heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = Some(heading.into());
        self
    }

    /// Text after the section, separated from it by a blank line.
    #[must_use]
    pub fn with_appended(mut self, section: impl Into<String>) -> Self {
        self.appended = Some(section.into());
        self
    }

    /// Lists only the tools `keep` answers `true` for, and the pinned ones.
    #[must_use]
    pub fn with_filter(
        mut self,
        keep: impl Fn(&ToolPresentationEntry<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filter = Some(Arc::new(keep));
        self
    }
}

impl ToolPresenter for WrappedToolPresenter {
    fn attachment_omission_text(
        &self,
        notice: &lash_protocol_types::AttachmentMaterializationNotice,
    ) -> String {
        match &self.omission_renderer {
            Some(render) => render(notice),
            None => self.inner.attachment_omission_text(notice),
        }
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal> {
        self.inner.admit_config(raw)
    }

    fn present(
        &self,
        request: ToolPresentationRequest<'_>,
    ) -> Result<PresentedToolSection, PresentationRefusal> {
        let (kept, filtered): (Vec<_>, Vec<_>) = request
            .tools
            .iter()
            .partition(|tool| tool.pinned || self.filter.as_ref().is_none_or(|keep| keep(tool)));
        let kept = kept.into_iter().copied().collect::<Vec<_>>();
        let decoration_bytes = [self.heading.as_deref(), self.appended.as_deref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .map(|text| text.len() + 2)
            .sum::<usize>();
        let mut section = self
            .inner
            .present(ToolPresentationRequest {
                tools: &kept,
                max_section_bytes: request
                    .max_section_bytes
                    .map(|limit| limit.saturating_sub(decoration_bytes)),
                ..request
            })
            .map_err(|refusal| match (refusal, request.max_section_bytes) {
                (PresentationRefusal::SectionLimit { required, .. }, Some(limit)) => {
                    PresentationRefusal::SectionLimit {
                        required: required + decoration_bytes,
                        limit,
                    }
                }
                (refusal, _) => refusal,
            })?;
        section
            .omitted
            .extend(filtered.into_iter().map(|tool| tool.manifest.id.clone()));
        let order = |id: &lash_vocabulary::tool_contract::ToolId| {
            request
                .tools
                .iter()
                .position(|tool| tool.manifest.id == *id)
        };
        section.omitted.sort_by_key(order);
        section.text = [
            self.heading.as_deref(),
            Some(section.text.as_str()),
            self.appended.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
        if let Some(limit) = request.max_section_bytes {
            let required = section.text.len();
            if required > limit {
                return Err(PresentationRefusal::SectionLimit { required, limit });
            }
        }
        Ok(section)
    }

    fn tool_doc(
        &self,
        tool: &ToolPresentationEntry<'_>,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        self.inner.tool_doc(tool, config)
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &JsonSchema,
        output: &JsonSchema,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        self.inner.tool_signature(call_path, input, output, config)
    }

    fn discovery_text(
        &self,
        manifest: &ToolManifest,
        contract: &ToolContract,
        config: &Value,
    ) -> Result<String, PresentationRefusal> {
        self.inner.discovery_text(manifest, contract, config)
    }
}
