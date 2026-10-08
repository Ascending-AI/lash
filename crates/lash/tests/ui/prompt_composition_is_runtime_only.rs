// Only the runtime builds a model call's cut and composes its prompt, at the
// call's admission (ADR 0133, FIG-5260): the facade exports neither the cut,
// its parts, the composed prompt nor the render pool, and a host reads its
// catalog and previews its plan but cannot compose or resolve one.
use lash::plugins::{ComposedPrompt, PromptCut, PromptCutParts, PromptRenderPool};

fn main() {
    let _ = lash::plugins::PromptCatalog::compose;
    let _ = lash::plugins::PromptCatalog::resolve;
}
