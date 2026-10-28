//! Editing code and fulfilling Imports are independent operations.
use lash_kernel_doc::Document;
use lash_kernel_package::{
    Attachment, Link, LinkRefusal, LinkedContent, Namespace, PublicationContext,
};
/// Fulfil a draft without mutating its body or any FunctionId.
pub fn relink(
    document: &Document,
    content: &LinkedContent,
    catalog: &dyn lash_kernel_doc::FunctionCatalog,
    namespace: &Namespace<'_>,
    context: &PublicationContext,
    attachment: Option<&Attachment>,
) -> Result<Link, LinkRefusal> {
    namespace.fulfil(
        content,
        catalog,
        document.identity(),
        context,
        None,
        attachment,
    )
}
