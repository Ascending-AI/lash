## Design Context

### Users

Engineers reading TUI design docs and architecture references during long
working sessions, often at night, often in dim physical contexts. They are
also the *operators* of the lash terminal agent itself — so the same audience
sees the docs (web), the TUI (terminal), and any future landing surface
(web). The job they're doing varies — *understand the runtime so I can change
it safely*, *learn the design language before contributing UI*, *use the
agent through a long task without my eyes burning out* — but the audience
sensibility is consistent: technical, opinionated, willing to read.

### Brand Personality

**Industrial, editorial, opinionated.** A workshop console and field
notebook, not a glossy SaaS dashboard. The interface is terse but not
sterile, refined but not precious. It should feel sturdy enough to hold a
long work session without becoming visually noisy. Sodium-orange announces
the active edge — prompt, plan, decisions — and almost nothing else gets
the spotlight.

Voice: declarative, lowercase headlines that don't try too hard, sentences
that earn their place. Reach for plain punctuation before em-dashes; an
em-dash should stay rare enough to mean something, never the default joint.
Concrete nouns over abstractions. No marketing voice anywhere.

### Aesthetic Direction

**Warm Iron / Fast Amber.** Warm-black surfaces (OKLCH neutral ramp tilted
to hue ~75), sodium-orange intent markers (`#E8A33C` ≈ `oklch(0.74 0.155
65)`), ash-grey structural lines, chalk text. Diagonal scoring and subtle
fractal-noise overlays keep the background from collapsing into pure black
emptiness. Big Shoulders Display for headings, Spectral for body, Chivo
Mono for labels and code.

**Theme: dark.** Derived from audience and viewing context — engineers in
dim rooms during long sessions. Not a default; a deliberate read of when
this product is actually used.

**Anti-references** (this is *not* what we're making):
- Vercel / Next.js / generic dev-tool docs (Inter + gradient hero + identical
  card grids).
- Plain GitHub README or wiki render — no design point of view.
- Linear / Notion / Stripe-flavored polished SaaS (serif body + sans heading +
  heavy whitespace + screenshot grid).

We are closer to: a 1970s mainframe terminal manual, a field service
notebook, a metal toolbox label, a museum exhibit caption set in industrial
signage type.

### Design Principles

1. **Accent means action.** Sodium is reserved for the active edge — prompts,
   key labels, active controls, headings that genuinely matter. If
   you're tempted to colorize a passive surface, use chalk hierarchy
   instead.
2. **Structure before ornament.** Every line, rail, and box should clarify
   hierarchy before it tries to look good. Beauty comes from discipline.
3. **Editorial breaks the grid.** A page with three card grids in a row is
   a template, not a design. At least one section per page must use a
   non-card layout — hanging numbers, hairline-divided lists, asymmetric
   prose. The chapter list and principle list are the canonical examples.
4. **Long sessions stay calm.** No high-saturation backgrounds, no fragile
   thin text, no ornamental overload. The palette and typography survive
   hours of reading.
5. **Cross-surface continuity.** The TUI, the docs, and any future
   landing/marketing site share the same brand vocabulary — sodium intent,
   ash structure, chalk text, the slash device, Big Shoulders Display
   headlines. If something looks at home in the docs but wrong in the TUI
   (or vice versa), one of them is straying.

### Constraints

- **Vanilla web stack** for docs — HTML / CSS / JS, no framework, no build
  step. Mermaid is loaded conditionally (only on pages with diagrams).
- **OKLCH tokens** for all color. No new hex literals outside literal
  swatch chips.
- **4pt spacing scale** with semantic names (`--space-*`). No raw px.
- **a11y is best-effort**, not a hard WCAG target — but `--ash-text` and
  `--chalk-dim` are tuned to clear AA against `--form` and shouldn't be
  dropped below their current lightness without re-checking contrast.
- **Anti-pattern guardrails** are non-negotiable. See
  [`docs/STYLEGUIDE.md`](./docs/STYLEGUIDE.md) for the full list — no
  glassmorphism on cards, no side-stripe borders, no gradient text, no new
  fonts, no auto-redirect stubs, no comma-stuffed list-as-sentence prose.
