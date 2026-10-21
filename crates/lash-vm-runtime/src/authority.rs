//! Trusted publication checks replace historical definition-membership authority.
use lash_kernel_package::{LinkRefusal, LinkedContent, Namespace, Package, PublicationContext};
/// The runtime supplies authenticated context and accepted publications.
/// A content digest or FunctionId alone grants no visibility.
pub fn admit_publication(
    content: &LinkedContent,
    context: &PublicationContext,
    package: &Package,
    snapshot: &Namespace<'_>,
) -> Result<(), LinkRefusal> {
    content.validate_publication(context, package, snapshot)
}
