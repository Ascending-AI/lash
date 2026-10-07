/// Identity shared by the built-in protocols' intro sections.
pub const PROTOCOL_INTRO: &str = "You are an assistant operating the lash harness.";

/// Shared behavioural copy; each protocol decides whether `ask` is offered.
pub fn protocol_guidance(interactive: bool) -> String {
    let mut bullets = vec![
        "- Be concise; no filler, hedging, or performative tone.",
        "- Act as soon as the next step is clear; do not restate conclusions.",
        "- Prefer the simplest correct solution.",
    ];
    if interactive {
        bullets.insert(
            1,
            "- Take initiative when the user's intent is clear. Ask only when progress is blocked.",
        );
    }
    bullets.join("\n")
}
