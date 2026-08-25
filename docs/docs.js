/* lash · docs · TOC builder + auto-knot + mermaid loader
   On pages with body.docs-page, build the multi-level TOC sidebar
   and highlight the current page. On the landing (no .docs-page),
   auto-knot each top-level .section/.stanza in .body. */

(function () {
  "use strict";

  const SCENE_ASSET_VERSION = "6";

  // Capture our own <script> element NOW, during initial top-level
  // execution: `document.currentScript` is valid here (a deferred
  // classic script), and crucially this runs before any SPA pushState
  // has rewritten `location`. We freeze the docs root off it in
  // TOC_BASE below — see the note there for why deriving it lazily is a
  // bug.
  const SELF_SCRIPT = document.currentScript;

  // ── docs registry ────────────────────────────────────────
  // The docs registry is the single canonical source for navigation,
  // pager order, landing cards, search page labels, and related-link
  // surfaces. hrefs are relative to /docs/ (top level);
  // architecture pages use "architecture/<name>.html".
  //
  // Moved/stub pages are intentionally omitted: they stay on disk for
  // external compatibility, but they must not appear in canonical nav,
  // pager order, landing cards, search, or related-link flows.
  const DOCS = [
    {
      label: "Start here",
      tier: "guide",
      summary: "Install the runtime, run a turn, and choose the next task path.",
      href: "quickstart.html",
      items: [
        {
          title: "Quickstart",
          href: "quickstart.html",
          summary: "Add the crate, configure a provider, open a session, and run one collected turn.",
          kind: "guide",
        },
        {
          title: "Task index",
          href: "tasks.html",
          summary: "Pick a common job: stream a turn, persist a session, add a tool, expose state, spawn subagents, run background work, or inspect traces.",
          kind: "guide",
        },
      ],
    },
    {
      label: "Execution modes",
      tier: "guide",
      summary: "Choose how the model acts each turn: native provider tool-calls (standard) or model-authored Lashlang programs (RLM). One commit unit either way.",
      href: "execution-modes.html",
      items: [
        {
          title: "Standard vs RLM",
          href: "execution-modes.html",
          summary: "The per-turn choice: standard native tool-calling vs model-authored Lashlang programs, how to select each, and when to reach for one over the other.",
          kind: "guide",
        },
        {
          title: "RLM protocol",
          href: "rlm.html",
          summary: "Prompt construction, history projection, variables, the execution loop, finish semantics, and failure modes for the RLM mode.",
          kind: "guide",
        },
      ],
    },
    {
      label: "Build an app",
      tier: "guide",
      summary: "Wire Lash into a product backend with sessions, turns, prompts, persistence, and traces.",
      href: "embedding.html",
      items: [
        {
          title: "API basics",
          href: "embedding.html",
          summary: "The core/session/turn facade and the host-owned runtime facets every app names explicitly.",
          kind: "guide",
        },
        {
          title: "Turns and streams",
          href: "embedding-turns.html",
          summary: "Turn outcomes, live semantic activity, streaming sinks, usage events, and RLM finish behavior.",
          kind: "guide",
        },
        {
          title: "Streaming and reconnect",
          href: "streaming.html",
          summary: "Turn-local streams, reconnectable session cursors, frontend folding, gap recovery, and remote activity framing.",
          kind: "guide",
        },
        {
          title: "Prompts and bindings",
          href: "embedding-prompts.html",
          summary: "Context strategy, prompt templates, typed plugin input, and projected RLM bindings.",
          kind: "guide",
        },
        {
          title: "Advanced runtime",
          href: "embedding-advanced.html",
          summary: "Session lifecycle, durable execution scopes, subagents, MCP servers, and advanced host controls.",
          kind: "guide",
        },
      ],
    },
    {
      label: "Tools",
      tier: "guide",
      summary: "Expose callable host operations to model turns and Lashlang modules.",
      href: "tools.html",
      items: [
        {
          title: "Tools overview",
          href: "tools.html",
          summary: "Advertise tools, execute calls, use ToolContext capabilities, and manage registry state.",
          kind: "guide",
        },
        {
          title: "Tool execution and durability",
          href: "tools-execution.html",
          summary: "Schedule tool calls, defer completion, borrow durable effect boundaries, and project large outputs.",
          kind: "guide",
        },
      ],
    },
    {
      label: "Plugins",
      tier: "guide",
      summary: "Add behavior that participates in runtime/session lifecycle, prompt construction, policy, or resumable state.",
      href: "plugins.html",
      items: [
        {
          title: "Plugin basics",
          href: "plugins.html",
          summary: "Implement PluginFactory and SessionPlugin when tools need prompt hooks, runtime hooks, typed input, catalog policy, or resumable session state.",
          kind: "guide",
        },
        {
          title: "Runtime plugins",
          href: "plugins-runtime.html",
          summary: "Persist plugin state, restore snapshots, and hook into runtime/session lifecycle.",
          kind: "reference",
        },
      ],
    },
    {
      label: "Operations",
      tier: "guide",
      summary: "Run Lash with durable stores, traces, workflow replay, scaling, and service boundaries.",
      href: "persistence.html",
      items: [
        {
          title: "Persistence",
          href: "persistence.html",
          summary: "Install local or distributed stores, understand what persists, handle session-lane conflicts, and verify Postgres/S3/Restate E2E durability.",
          kind: "guide",
        },
        {
          title: "Tracing",
          href: "tracing.html",
          summary: "Attach TraceSink, write JSONL, and inspect Lashlang execution graphs.",
          kind: "guide",
        },
        {
          title: "Reporting channels",
          href: "reporting.html",
          summary: "Map the reporting channels — SessionEvent, TurnActivity, TraceSink, ProcessEventSink, the remote wire mirror — with roles, the runtime-vs-surface tool-name mapping, and the schema-evolution policy.",
          kind: "reference",
        },
        {
          title: "Durability and replay",
          href: "architecture/durability.html",
          summary: "EffectHost, ScopedEffectController, workflow replay, idempotent commits, and durable background work.",
          kind: "guide",
        },
        {
          title: "Scaling",
          href: "architecture/scaling.html",
          summary: "Run Lash as a stateless service with Postgres, session execution leases, S3-compatible attachments, Restate replay, and session-agnostic ingress.",
          kind: "guide",
        },
        {
          title: "Running in production",
          href: "operations.html",
          summary: "Compose the host-owned operational levers: deployment topologies, worker identity, lease timings, graceful drain, failure classification, backpressure, and monitoring.",
          kind: "guide",
        },
        {
          title: "Prompt-cache diagnosis",
          href: "cache-diagnosis.html",
          summary: "Detect prompt-cache regressions by lane, inspect request and response evidence, and map failures to executable regression tests.",
          kind: "runbook",
        },
        {
          title: "Remote protocol",
          href: "remote-protocol.html",
          summary: "Versioned DTOs for HTTP, queues, callbacks, workflow handlers, activity streams, triggers, and host-owned tool grants.",
          kind: "reference",
        },
      ],
    },
    {
      label: "Examples",
      tier: "guide",
      summary: "See complete hosts that combine runtime, product state, tools, UI, persistence, and workflow edges.",
      href: "examples.html",
      items: [
        {
          title: "Examples overview",
          href: "examples.html",
          summary: "Choose between the browser agent service and the process-heavy workbench walkthrough.",
          kind: "guide",
        },
        {
          title: "Agent service",
          href: "example-agent-service.html",
          summary: "Browser chat app with RLM, streaming, board tools, SQLite app state, and optional Restate turns.",
          kind: "example",
        },
        {
          title: "Agent workbench",
          href: "example-agent-workbench.html",
          summary: "Trigger, process, cron, mail, and queued-work example for background-capable hosts.",
          kind: "example",
        },
      ],
    },
    {
      label: "Reference",
      tier: "reference",
      summary: "Jump to API indices, the Lashlang language spec, and low-level supporting references.",
      href: "architecture/reference.html",
      items: [
        {
          title: "API reference",
          href: "architecture/reference.html",
          summary: "Rustdoc index for facade, core, provider, protocol, store, trace, export, and example crates.",
          kind: "reference",
        },
        {
          title: "Lashlang language",
          href: "architecture/lashlang.html",
          summary: "The CodeAct language behind RLM: syntax, module shape, foreground blocks, processes, labels, and language-level diagnostics.",
          kind: "reference",
        },
        {
          title: "Providers",
          href: "architecture/providers.html",
          summary: "Provider request/response normalization, cache policy, usage mapping, and adding a provider crate.",
          kind: "reference",
        },
        {
          title: "Dependency map",
          href: "architecture/deps.html",
          summary: "Workspace dependency boundaries and crate-layer constraints.",
          kind: "reference",
        },
      ],
    },
    {
      label: "Internals",
      tier: "internals",
      summary: "Understand why the SDK is split into facade, core, protocol, language, provider, store, trace, and tooling crates.",
      href: "architecture/index.html",
      items: [
        {
          title: "System paths",
          href: "architecture/index.html",
          summary: "Entry points into the architecture docs for builders, contributors, and debuggers.",
          kind: "internal",
        },
        {
          title: "System overview",
          href: "architecture/overview.html",
          summary: "The runtime boundary, session graph, host effects, plugins, providers, and stores at a glance.",
          kind: "internal",
        },
        {
          title: "Kernel surface",
          href: "architecture/kernel-surface.html",
          summary: "The explicit lash-core kernel contract: primitive mechanisms, extension traits, and protocol/process boundaries.",
          kind: "internal",
        },
        {
          title: "Crate map",
          href: "architecture/modules.html",
          summary: "Workspace module boundaries and which crates own which responsibilities.",
          kind: "internal",
        },
        {
          title: "Crate invariants",
          href: "architecture/crate-invariants.html",
          summary: "Falsifiable dependency and ownership boundaries for every workspace crate.",
          kind: "internal",
        },
        {
          title: "Data flow",
          href: "architecture/flow.html",
          summary: "How input, prompts, LLM effects, tool effects, graph commits, and projections move through a turn.",
          kind: "internal",
        },
        {
          title: "Execution",
          href: "architecture/execution.html",
          summary: "Turn machine phases, protocol drivers, stream projection, and runtime execution boundaries.",
          kind: "internal",
        },
        {
          title: "Lashlang effects",
          href: "architecture/lashlang-effects.html",
          summary: "Executor effects, projected host bindings, trigger registry, process wiring, and module artifacts.",
          kind: "internal",
        },
        {
          title: "Lashlang runtime",
          href: "architecture/lashlang-runtime.html",
          summary: "Parser, linker, compiler, VM, bytecode, artifact cache, and runtime execution internals.",
          kind: "internal",
        },
        {
          title: "Runtime host",
          href: "architecture/runtime.html",
          summary: "Runtime environment, host services, session lifecycle, residency, queued work, and effects.",
          kind: "internal",
        },
        {
          title: "Contracts",
          href: "architecture/abstractions.html",
          summary: "Core abstractions and invariants across sessions, plugins, tools, providers, and stores.",
          kind: "internal",
        },
        {
          title: "Triggers",
          href: "architecture/triggers.html",
          summary: "Session-agnostic ingress: declare, emit, match by source_type, idempotent delivery, and the wake that is the only session-ordered step. Timers and cron as trigger sources.",
          kind: "internal",
        },
      ],
    },
  ];

  const MOVED_STUB_HREFS = new Set([
    "architecture.html",
    "architecture/execution-modes.html",
    "plugins-tools.html",
  ]);

  function normalizeDocHref(href) {
    if (!href) return "";
    let out = String(href).trim();
    out = out.replace(/^[.]\//, "");
    out = out.replace(/^\/docs\//, "");
    out = out.replace(/^docs\//, "");
    out = out.replace(/[?#].*$/, "");
    if (out === "" || out === "/") return "index.html";
    out = out.replace(/^\//, "");
    if (out.endsWith("/")) out += "index.html";
    if (out === "architecture/") return "architecture/index.html";
    return out;
  }

  function flatDocs() {
    const out = [];
    for (const group of DOCS) {
      for (const item of group.items || []) {
        const href = normalizeDocHref(item.href);
        if (!href || MOVED_STUB_HREFS.has(href)) continue;
        out.push({
          ...item,
          href,
          track: group.label,
          trackSummary: group.summary,
        });
      }
    }
    return out;
  }

  const DOCS_BY_HREF = new Map(flatDocs().map((page) => [page.href, page]));

  // Keep the old TOC shape as a view of the registry; renderers below
  // consume TOC so the nav code stays small.
  const TOC = DOCS.map((group) => ({
    label: group.label,
    tier: group.tier || "guide",
    summary: group.summary,
    href: normalizeDocHref(group.href),
    items: (group.items || [])
      .filter((item) => !MOVED_STUB_HREFS.has(normalizeDocHref(item.href)))
      .map((item) => ({
        name: item.title,
        title: item.title,
        href: normalizeDocHref(item.href),
        summary: item.summary,
        kind: item.kind,
      })),
  }));

  // Flatten the TOC into a single ordered list for pager auto-fill and
  // any other code that wants the canonical linear order.
  function flatTOC() {
    return flatDocs().map((page) => ({
      name: page.title,
      title: page.title,
      href: page.href,
      summary: page.summary,
      kind: page.kind,
      track: page.track,
    }));
  }

  // ── helpers ─────────────────────────────────────────────
  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function currentPath() {
    // Return the path relative to the docs root (tocBase), so it lines
    // up with the TOC's doc-relative hrefs at ANY deploy root — "/",
    // "/docs/", or a channel prefix like "/main/". Falls back to a
    // /docs/ scan, then a bare leading-slash strip, for legacy setups.
    let p = location.pathname;
    const base = tocBase();
    if (base && base !== "/" && p.startsWith(base)) {
      p = p.slice(base.length);
    } else {
      const idx = p.indexOf("/docs/");
      if (idx >= 0) p = p.slice(idx + 6);
      else p = p.replace(/^\//, "");
    }
    if (p === "" || p.endsWith("/")) p += "index.html";
    return p;
  }

  // Absolute URL path of the docs ROOT, so TOC links + dynamic script
  // injections (mermaid loader) + search-index fetches resolve from any
  // page no matter how deeply nested. Derived from docs.js's own
  // <script src> — wherever this file lives is, by definition, the docs
  // root — and COMPUTED ONCE, at load time, then cached.
  //
  // It must be captured before any SPA pushState runs. `spaNavigate`
  // rewrites `location` via history.pushState but never swaps the
  // <head> <script> tags, so the docs.js element keeps its original
  // relative `src` ("./docs.js" on a root page, "../docs.js" under
  // architecture/). Re-resolving that relative src against the *live*
  // location after a cross-directory nav yields the wrong root (e.g.
  // "/architecture/" instead of "/") — which 404s the injected
  // mermaid loader and every search-index fetch. That was the
  // "diagrams don't render / ⌘K finds nothing until a hard refresh"
  // bug: a hard refresh is a full load, so the base resolved correctly.
  const TOC_BASE = (function () {
    // 1) our own script element, resolved against the load-time URL.
    if (SELF_SCRIPT) {
      const src = SELF_SCRIPT.getAttribute("src") || "";
      try {
        return new URL(src, location.href).pathname.replace(/[^/]*$/, "");
      } catch (e) { /* fall through */ }
    }
    // 2) scan for any docs.js script (unexpected embeddings).
    const scripts = document.querySelectorAll('script[src]');
    for (const s of scripts) {
      const src = s.getAttribute("src") || "";
      if (/(^|\/)docs\.js(?:\?|$)/.test(src)) {
        try {
          return new URL(src, location.href).pathname.replace(/docs\.js$/, "");
        } catch (e) { /* fall through */ }
      }
    }
    // 3) /docs/ path scan, then root — legacy fallbacks.
    const p = location.pathname;
    const idx = p.indexOf("/docs/");
    if (idx >= 0) return p.slice(0, idx + 6);
    return "/";
  })();

  function tocBase() {
    return TOC_BASE;
  }

  // ── TOC builder ─────────────────────────────────────────
  // Groups default to COLLAPSED — only the group containing the
  // active page auto-opens, plus any group the user has explicitly
  // expanded. Storage tracks the "expanded" set so user clicks
  // persist across navigations.
  const EXPANDED_KEY = "lash-toc-expanded";
  const CARET_SVG = '<svg class="toc__caret" viewBox="0 0 12 12" width="12" height="12" aria-hidden="true"><path d="M3.5 2 L8 6 L3.5 10" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>';

  function getExpandedGroups() {
    try {
      const raw = localStorage.getItem(EXPANDED_KEY);
      return raw ? new Set(JSON.parse(raw)) : new Set();
    } catch (e) { return new Set(); }
  }
  function saveExpandedGroups(set) {
    try { localStorage.setItem(EXPANDED_KEY, JSON.stringify([...set])); }
    catch (e) { /* ignore quota / private mode */ }
  }

  // ── sections (top-bar tabs) ─────────────────────────────
  // The top band carries one tab per section; the left sidebar shows
  // only the active section's groups. Guide is the long primary spine;
  // Reference, Architecture, and CLI are their own short sections reached
  // from the band. This keeps the guide spine continuous and drops the
  // in-rail tier dividers entirely.
  const TIER_ORDER = ["guide", "reference", "internals"];
  const TIER_LABELS = {
    guide: "Guide",
    reference: "Reference",
    internals: "Internals",
  };
  // True if `href` is any leaf under this nav node.
  function nodeHasHref(node, href) {
    if (node.items) return node.items.some((c) => nodeHasHref(c, href));
    return node.href === href;
  }
  // The section that owns a page (defaults to guide for the landing /
  // anything not in the registry).
  function tierForHref(href) {
    for (const g of TOC) if (nodeHasHref(g, href)) return g.tier || "guide";
    return "guide";
  }
  // Where a section tab points: the first group's href in that section,
  // falling back to its first item.
  function tierLandingHref(tier) {
    for (const g of TOC) {
      if ((g.tier || "guide") !== tier) continue;
      if (g.href) return g.href;
      if (g.items && g.items[0] && g.items[0].href) return g.items[0].href;
    }
    return "index.html";
  }
  function bandSectionsHTML(base, activeTier) {
    const at = activeTier || tierForHref(currentPath());
    return (
      '<nav class="band__sections" aria-label="documentation sections">' +
      TIER_ORDER.map((t) => {
        const cls = "band__section" + (t === at ? " is-active" : "");
        const aria = t === at ? ' aria-current="page"' : "";
        return (
          `<a class="${cls}" data-tier="${t}"${aria} ` +
          `href="${escapeHtml(base + tierLandingHref(t))}">` +
          `${escapeHtml(TIER_LABELS[t])}</a>`
        );
      }).join("") +
      "</nav>"
    );
  }
  // Keep the band's active tab in sync after an SPA nav without rebuilding it.
  function syncBandSections(activeTier) {
    const nav = document.querySelector(".band__sections");
    if (!nav) return;
    const at = activeTier || tierForHref(currentPath());
    nav.querySelectorAll(".band__section").forEach((a) => {
      const on = a.getAttribute("data-tier") === at;
      a.classList.toggle("is-active", on);
      if (on) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
  }

  function buildTOC() {
    const host = document.querySelector(".toc");
    if (!host) return;
    const cur = currentPath();
    const base = tocBase();
    const expanded = getExpandedGroups();

    // True if any direct or nested item under `group` matches `href`.
    function containsHref(group, href) {
      if (!group.items) return false;
      return group.items.some((it) =>
        it.items ? containsHref(it, href) : it.href === href
      );
    }

    // Render a single child of a group: either a leaf <li><a> or a
    // nested subgroup with its own small heading and indented list.
    function renderChild(item) {
      if (item.items) {
        const subActive = containsHref(item, cur);
        const subClasses = "toc__subgroup" + (subActive ? " is-active" : "");
        const lines = [];
        lines.push(`<li class="${subClasses}">`);
        lines.push(`<p class="toc__sub-label">${escapeHtml(item.label)}</p>`);
        lines.push(`<ol class="toc__subitems">`);
        for (const child of item.items) lines.push(renderChild(child));
        lines.push(`</ol>`);
        lines.push(`</li>`);
        return lines.join("");
      }
      const isActive = item.href === cur;
      return (
        `<li><a${isActive ? ' class="is-active" aria-current="page"' : ""} ` +
        `href="${escapeHtml(base + item.href)}">${escapeHtml(item.name)}</a></li>`
      );
    }

    const out = [];
    // The groups live inside a native <details> disclosure. On desktop
    // (>=981px) the CSS hides the <summary> and forces the panel open, so
    // it reads exactly like the old always-on sticky sidebar. On narrow
    // viewports (<=980px) the disclosure is collapsed by default so the
    // reader hits the <h1> immediately instead of scrolling past the
    // whole multi-group nav. `open` is reconciled live by syncDisclosure().
    out.push('<details class="toc-disclosure">');
    out.push(
      '<summary class="toc-disclosure__summary">' +
        `<span class="toc-disclosure__caret" aria-hidden="true">${CARET_SVG}</span>` +
        '<span class="toc-disclosure__label">Contents</span>' +
      '</summary>'
    );
    out.push('<div class="toc-disclosure__panel">');
    // The sidebar shows only the active section's groups; the top band
    // switches sections. No in-rail dividers — each section is its own
    // self-contained rail, and the guide spine stays continuous.
    const activeTier = tierForHref(cur);
    window.__LASH_ACTIVE_TIER = activeTier;
    for (const group of TOC) {
      if ((group.tier || "guide") !== activeTier) continue;
      const containsActive = containsHref(group, cur);
      const groupActive = containsActive;
      const hasItems = group.items && group.items.length > 0;
      // Groups default to collapsed. Auto-open the group containing
      // the active page (so users see where they are), plus any
      // groups the reader has explicitly expanded previously.
      const isCollapsed = hasItems && !containsActive && !expanded.has(group.label);
      const classes = [
        "toc__group",
        hasItems ? "toc__group--collapsible" : "",
        isCollapsed ? "is-collapsed" : "",
      ].filter(Boolean).join(" ");
      out.push(`<nav class="${classes}" data-group="${escapeHtml(group.label)}">`);
      const titleClasses = "toc__group-title" + (groupActive ? " is-active" : "");
      // Title is a button when the group has items — the whole row
      // toggles collapse, not just the caret. The caret stays as a
      // visual indicator that rotates with the group's open state.
      if (hasItems) {
        out.push(
          `<button class="${titleClasses}" type="button" aria-expanded="${!isCollapsed}">` +
            `<span class="toc__caret-wrap" aria-hidden="true">${CARET_SVG}</span>` +
            `<span class="toc__group-title-text">${escapeHtml(group.label)}</span>` +
          `</button>`
        );
        out.push('<ol class="toc__items">');
        for (const item of group.items) out.push(renderChild(item));
        out.push("</ol>");
      } else {
        out.push(
          `<p class="${titleClasses}">` +
            `<span class="toc__caret-spacer" aria-hidden="true"></span>` +
            `<span class="toc__group-title-text">${escapeHtml(group.label)}</span>` +
          `</p>`
        );
      }
      out.push(`</nav>`);
    }
    out.push('</div>'); // .toc-disclosure__panel
    out.push('</details>');
    host.innerHTML = out.join("\n");
    syncBandSections(activeTier);

    // ── reconcile the disclosure's open state with the viewport ──
    //    Desktop: always open (CSS also hides the summary + forces the
    //    panel visible, so `open` is belt-and-braces but keeps the DOM
    //    honest). Mobile: closed by default so content is reachable; the
    //    reader toggles it via the "Contents" summary or the floating
    //    "back to contents" button. A matchMedia listener keeps this
    //    correct across live resize without rebuilding the TOC.
    const disclosure = host.querySelector(".toc-disclosure");
    if (disclosure) {
      const mq = window.matchMedia("(max-width: 980px)");
      const syncDisclosure = (isNarrow) => {
        // Desktop: force-open (the CSS also hides the summary + always
        // lays out the panel, so the sidebar matches the old layout).
        // Mobile: collapse to content-first. This fires only when the
        // match state actually flips across 980px (initial sync + the
        // 'change' listener), so a reader who opened the disclosure on
        // mobile isn't slammed shut by ordinary scroll/resize ticks.
        disclosure.open = !isNarrow;
        // redraw the spine: the panel's height changes as it opens/closes
        drawSpineSnake();
      };
      syncDisclosure(mq.matches);
      // addEventListener('change', …) is the modern MediaQueryList API;
      // it only fires when the match state actually flips across 980px.
      mq.addEventListener("change", (e) => syncDisclosure(e.matches));
      // Toggling the native disclosure (mobile) shifts panel height too.
      disclosure.addEventListener("toggle", () => drawSpineSnake());
    }

    // wire title toggles — the whole title row is the toggle button
    host.querySelectorAll("button.toc__group-title").forEach((btn) => {
      btn.addEventListener("click", (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        const group = btn.closest(".toc__group");
        if (!group) return;
        const label = group.getAttribute("data-group");
        const nowCollapsed = group.classList.toggle("is-collapsed");
        btn.setAttribute("aria-expanded", String(!nowCollapsed));
        const current = getExpandedGroups();
        if (nowCollapsed) current.delete(label);
        else current.add(label);
        saveExpandedGroups(current);
        // redraw spine since group heights changed
        drawSpineSnake();
      });
    });
  }

  // ── pager auto-fill from neighbours in the flat page list ──
  function buildPager() {
    const host = document.querySelector(".pager__main");
    if (!host) return;
    const flat = flatTOC();
    const cur = currentPath();
    const idx = flat.findIndex(e => e.href === cur);
    if (idx < 0) return;
    const base = tocBase();
    const prev = flat[idx - 1];
    const next = flat[idx + 1];
    const prevHtml = prev
      ? `<a class="pager__prev" href="${escapeHtml(base + prev.href)}">` +
        `<span class="arrow" aria-hidden="true">←</span>` +
        `<span>previous</span><strong>${escapeHtml(prev.name)}</strong>` +
        `</a>`
      : '<span></span>';
    const nextHtml = next
      ? `<a class="pager__next" href="${escapeHtml(base + next.href)}">` +
        `<span>next</span><strong>${escapeHtml(next.name)}</strong>` +
        `<span class="arrow" aria-hidden="true">→</span>` +
        `</a>`
      : '<span></span>';
    host.innerHTML = prevHtml + nextHtml;
  }

  function pageForHref(href) {
    return DOCS_BY_HREF.get(normalizeDocHref(href)) || null;
  }

  function trackEntryPage(group) {
    return pageForHref(group.href) || pageForHref((group.items && group.items[0] && group.items[0].href) || "");
  }

  function mountLandingRegistry() {
    const host = document.querySelector("[data-docs-landing]");
    if (!host) return;
    const base = tocBase();
    const rows = [];
    for (const group of DOCS) {
      const page = trackEntryPage(group);
      if (!page) continue;
      rows.push(
        `<a class="listing__row docs-registry-row" href="${escapeHtml(base + page.href)}">` +
          `<div class="listing__label">${escapeHtml(group.label)}</div>` +
          `<div class="listing__body">` +
            `<p>${escapeHtml(group.summary || page.summary || "")}</p>` +
            `<span class="docs-registry-row__meta">${escapeHtml(page.title)} &middot; ${escapeHtml(page.kind || "page")}</span>` +
          `</div>` +
        `</a>`
      );
    }
    host.innerHTML = rows.join("");
  }

  function mountRelatedLinks() {
    const hosts = document.querySelectorAll("[data-related]");
    if (!hosts.length) return;
    const base = tocBase();
    hosts.forEach((host) => {
      const refs = (host.getAttribute("data-related") || "")
        .split(/[,\s]+/)
        .map((ref) => ref.trim())
        .filter(Boolean);
      const pages = [];
      const seen = new Set();
      refs.forEach((ref) => {
        const page = pageForHref(ref);
        if (!page || seen.has(page.href)) return;
        seen.add(page.href);
        pages.push(page);
      });
      if (!pages.length) {
        host.innerHTML = "";
        return;
      }
      if (!host.getAttribute("aria-label")) {
        host.setAttribute("aria-label", "related documentation");
      }
      host.classList.add("related-links");
      host.innerHTML = pages.map((page) =>
        `<a class="related-links__item" href="${escapeHtml(base + page.href)}">` +
          `<span class="related-links__kind">${escapeHtml(page.kind || "page")}</span>` +
          `<strong>${escapeHtml(page.title)}</strong>` +
          `<span>${escapeHtml(page.summary || "")}</span>` +
        `</a>`
      ).join("");
    });
  }

  // ── auto-knot — only on the landing (no .docs-page class) ──
  function autoKnot() {
    if (document.body.classList.contains("docs-page")) return;
    const body = document.querySelector(".body");
    if (!body) return;
    const items = body.querySelectorAll(":scope > section, :scope > .section, :scope > .stanza");
    let idx = 0;
    items.forEach((item) => {
      if (item.querySelector(":scope > .stanza__rail, :scope > .section__rail")) return;
      idx += 1;
      let knotName = item.dataset.knot || "";
      if (!knotName) {
        const h2 = item.querySelector("h2");
        if (h2) knotName = h2.textContent.replace(/[\.•·…]+\s*$/u, "").trim();
      }
      const knotId = "§ " + String(idx).padStart(2, "0");
      const rail = document.createElement("div");
      rail.className = "stanza__rail";
      rail.innerHTML =
        '<div class="knot">' +
          '<span class="knot__bead" aria-hidden="true"></span>' +
          '<p class="knot__id">' + escapeHtml(knotId) + '</p>' +
          (knotName ? '<p class="knot__name">' + escapeHtml(knotName.toLowerCase()) + '</p>' : '') +
        '</div>';
      item.prepend(rail);
    });
  }

  // ── mermaid loader — only if the page contains .mermaid elements ──
  // bumped MERMAID_LOADER_VERSION when mermaid.js changes so static
  // caches don't keep serving the old loader
  const MERMAID_LOADER_VERSION = "4";
  function loadMermaidIfNeeded() {
    if (!document.querySelector(".mermaid")) return;
    // Loader already running? `__lashMermaidRerender` is published
    // synchronously the moment mermaid.js's IIFE executes — well before
    // the CDN import resolves and sets `__lashMermaid`. The old guard
    // checked only `__lashMermaid`, so on a full load of a page that
    // ships mermaid.js in its own <head> (the architecture pages do) it
    // raced the import and injected a SECOND copy. Two mermaid instances
    // then fight over the same .mermaid nodes (reset-to-source + re-run),
    // which throws and can leave a diagram as raw text — a second source
    // of "diagrams sometimes don't render".
    if (window.__lashMermaid || typeof window.__lashMermaidRerender === "function") return;
    if (document.querySelector('script[data-lash-mermaid]')) return;
    // mermaid.js may already be on the page via the per-page HTML
    // <script> even before its IIFE has run — don't duplicate it.
    const onPage = [...document.querySelectorAll('script[src]')].some(
      (s) => /(^|\/)mermaid\.js(?:\?|$)/.test(s.getAttribute("src") || "")
    );
    if (onPage) return;
    const base = tocBase();
    const s = document.createElement("script");
    s.src = base + "mermaid.js?v=" + MERMAID_LOADER_VERSION;
    s.defer = true;
    s.setAttribute("data-lash-mermaid", "");
    document.head.appendChild(s);
  }

  // ── spine thread — a thin vertical sodium rail down the left edge
  //    of the TOC. No eyelets; active/hover affordances live on the
  //    item rows themselves now (background + bold text).
  function drawSpineSnake() {
    if (!document.body.classList.contains("docs-page")) return;
    const refToc = document.querySelector(".toc");
    if (!refToc) return;
    const prior = refToc.querySelector(":scope > .spine-snake");
    if (prior) prior.remove();

    const tocRect = refToc.getBoundingClientRect();
    // anchorX is the X (relative to .toc) where the spine sits. It is
    // a small fixed gutter inside the TOC's left edge — the spine acts
    // as a left rail, with everything (titles + items) sitting indented
    // to the right of it. Driven by a CSS variable so it can't drift.
    const anchorX = (function () {
      const rootCS = getComputedStyle(document.documentElement);
      const remPx = parseFloat(rootCS.fontSize) || 16;
      const railVar = rootCS.getPropertyValue("--toc-rail-x").trim();
      const rail = parseFloat(railVar);
      if (!Number.isNaN(rail) && rail > 0) {
        // value is in rem if it ends with rem, else px
        return railVar.endsWith("rem") ? rail * remPx : rail;
      }
      return 0.375 * remPx; // ~6px fallback
    })();
    const H = Math.max(refToc.scrollHeight, refToc.clientHeight);
    if (H < 100) return;

    const W = 28;
    const mainX = 6;

    const NS = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(NS, "svg");
    svg.setAttribute("class", "spine-snake");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
    svg.style.left = (anchorX - mainX) + "px";
    svg.style.width = W + "px";
    svg.style.height = H + "px";

    const thread = document.createElementNS(NS, "path");
    // Reversed M…L — start at the BOTTOM so a stroke-dashoffset
    // animation draws from the brazier-strand attachment point UP
    // through the page. Visually continues the lash that bloomed
    // out of the brazier at the bottom of the cover SVG.
    thread.setAttribute("d", `M ${mainX} ${H.toFixed(2)} L ${mainX} 0`);
    thread.setAttribute("class", "spine-thread");
    thread.setAttribute("fill", "none");
    thread.setAttribute("stroke", "currentColor");
    thread.setAttribute("stroke-width", "1");
    thread.setAttribute("vector-effect", "non-scaling-stroke");
    svg.appendChild(thread);

    refToc.insertBefore(svg, refToc.firstChild);

    // brazier strand at the bottom of the cover still aims at this X
    window.__LASH_SNAKE_ANCHOR_X = tocRect.left + anchorX;
  }

  // ── floating "back to contents" button — appears after the TOC
  //    has scrolled out of view; clicking jumps to the top
  function mountTocJump() {
    if (!document.body.classList.contains("docs-page")) return;
    const toc = document.querySelector(".toc");
    if (!toc) return;
    let btn = document.getElementById("toc-jump");
    if (!btn) {
      btn = document.createElement("button");
      btn.id = "toc-jump";
      btn.className = "toc-jump";
      btn.type = "button";
      btn.setAttribute("aria-label", "back to contents");
      btn.innerHTML =
        '<span class="arrow" aria-hidden="true">↑</span>' +
        '<span>contents</span>';
      btn.addEventListener("click", () => {
        // On mobile the TOC is a collapsed <details> disclosure — a plain
        // scroll-to-top would just land on a closed "Contents" summary.
        // Open it (and bring it into view) so the reader actually sees the
        // nav. On desktop there's no collapsed disclosure, so this is a
        // no-op and we fall back to the original scroll-to-top.
        const disclosure = toc.querySelector(".toc-disclosure");
        const narrow = window.matchMedia("(max-width: 980px)").matches;
        if (disclosure && narrow && !disclosure.open) {
          disclosure.open = true;
        }
        if (narrow) {
          toc.scrollIntoView({ behavior: "smooth", block: "start" });
        } else {
          window.scrollTo({ top: 0, behavior: "smooth" });
        }
      });
      document.body.appendChild(btn);
    }
    function tick() {
      const r = toc.getBoundingClientRect();
      // visible when the TOC's bottom is above the viewport
      const past = r.bottom < 24;
      btn.classList.toggle("is-visible", past);
    }
    tick();
    window.addEventListener("scroll", tick, { passive: true });
    window.addEventListener("resize", tick);
  }

  // ── SPA navigation — fetch + swap so the sidebar stays put ──
  //    Clicking a TOC link (or any internal docs link) fetches the
  //    target page, swaps the per-page sections in place, and updates
  //    the URL via pushState. The sticky TOC keeps its scroll and
  //    spine state; only the content area refreshes.
  function refreshTOCActive() {
    // Compare resolved pathnames directly so active-state works at any
    // deploy root (/, /docs/, /main/, …) rather than only under /docs/.
    const norm = (p) => p.replace(/\/$/, "/index.html");
    const curPath = norm(location.pathname);
    document.querySelectorAll(".toc__items a").forEach((a) => {
      let linkPath = curPath;
      try { linkPath = norm(new URL(a.getAttribute("href"), location.href).pathname); }
      catch (e) { linkPath = ""; }
      const match = linkPath === curPath;
      a.classList.toggle("is-active", match);
      if (match) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    document.querySelectorAll(".toc__group").forEach((g) => {
      const title = g.querySelector(".toc__group-title");
      if (!title) return;
      const hasActive = !!g.querySelector(".toc__items a.is-active");
      title.classList.toggle("is-active", hasActive);
      // auto-open the group holding the active page after an SPA nav,
      // mirroring buildTOC's initial-load behaviour — otherwise the
      // active page + its outline stay hidden in a collapsed group.
      if (hasActive && g.classList.contains("is-collapsed")) {
        g.classList.remove("is-collapsed");
        const btn = g.querySelector("button.toc__group-title");
        if (btn) btn.setAttribute("aria-expanded", "true");
      }
    });
  }

  // ── per-page outline ────────────────────────────────────
  //    The active page's sections + subsections, injected under its
  //    sidebar entry so subsections are reachable from the left rail
  //    (not only via ⌘K). Built from the live .body each navigation.
  function slugify(text) {
    return (String(text).toLowerCase().replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "").slice(0, 48)) || "section";
  }

  // Give every section + subsection heading a stable id so it can be a
  // deep-link / outline target. Top-level sections usually already have
  // one; h3 subsections frequently don't.
  function ensureHeadingIds() {
    const body = document.querySelector(".body");
    if (!body) return;
    const used = new Set();
    body.querySelectorAll("[id]").forEach((el) => used.add(el.id));
    const unique = (base) => {
      let id = base, n = 2;
      while (used.has(id)) id = base + "-" + n++;
      used.add(id);
      return id;
    };
    body.querySelectorAll(":scope > .section, :scope > section, :scope > .stanza").forEach((sec) => {
      if (sec.id) return;
      const h2 = sec.querySelector("h2");
      if (h2) sec.id = unique(slugify(h2.textContent));
    });
    body.querySelectorAll("h3").forEach((h) => {
      if (!h.id) h.id = unique(slugify(h.textContent));
    });
  }

  // Read the current page's outline: each top-level section (h2) plus
  // its nested h3 subsections.
  function pageOutline() {
    const body = document.querySelector(".body");
    if (!body) return [];
    const out = [];
    body.querySelectorAll(":scope > .section, :scope > section, :scope > .stanza").forEach((sec) => {
      const h2 = sec.querySelector("h2");
      if (!h2 || !sec.id) return;
      const subs = [];
      sec.querySelectorAll("h3[id]").forEach((h3) => {
        subs.push({ id: h3.id, title: h3.textContent.trim() });
      });
      out.push({ id: sec.id, title: h2.textContent.trim(), subs });
    });
    return out;
  }

  function buildOutlineHtml(outline) {
    const lines = ['<ol class="toc__outline">'];
    for (const s of outline) {
      lines.push('<li class="toc__outline-sec">');
      lines.push(`<a href="#${escapeHtml(s.id)}" data-outline="${escapeHtml(s.id)}">${escapeHtml(s.title.toLowerCase())}</a>`);
      if (s.subs.length) {
        lines.push('<ol class="toc__outline-subs">');
        for (const sub of s.subs) {
          lines.push(`<li><a href="#${escapeHtml(sub.id)}" data-outline="${escapeHtml(sub.id)}">${escapeHtml(sub.title.toLowerCase())}</a></li>`);
        }
        lines.push("</ol>");
      }
      lines.push("</li>");
    }
    lines.push("</ol>");
    return lines.join("");
  }

  let outlineSpyCleanup = null;
  function injectPageOutline() {
    document.querySelectorAll(".toc__outline").forEach((n) => n.remove());
    if (outlineSpyCleanup) { outlineSpyCleanup(); outlineSpyCleanup = null; }
    if (!document.body.classList.contains("docs-page")) return;
    ensureHeadingIds();
    const active = document.querySelector(".toc__items a.is-active");
    if (!active) return;
    const outline = pageOutline();
    if (outline.length < 2) return; // a single section isn't worth an outline
    active.insertAdjacentHTML("afterend", buildOutlineHtml(outline));
    wireOutlineSpy();
  }

  // Highlight the section/subsection currently at the top of the
  // reading area as the page scrolls.
  function wireOutlineSpy() {
    const links = Array.prototype.slice.call(
      document.querySelectorAll(".toc__outline a[data-outline]"));
    if (!links.length) return;
    const targets = links
      .map((a) => ({ link: a, el: document.getElementById(a.getAttribute("data-outline")) }))
      .filter((t) => t.el);
    let raf = 0;
    function update() {
      raf = 0;
      let current = null;
      const probe = 140; // px below the viewport top = the reading line
      for (const t of targets) {
        if (t.el.getBoundingClientRect().top - probe <= 0) current = t;
        else break;
      }
      if (!current && targets.length) current = targets[0];
      links.forEach((a) => a.classList.toggle("is-current", !!current && a === current.link));
    }
    function onScroll() { if (!raf) raf = requestAnimationFrame(update); }
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll, { passive: true });
    update();
    outlineSpyCleanup = function () {
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
    };
  }

  // mermaid.js owns its own theme + re-render lifecycle. After an SPA
  // content swap we just nudge it to pick up any newly-inserted .mermaid
  // blocks.
  function runMermaidIfReady() {
    if (typeof window.__lashMermaidRerender === "function") {
      window.__lashMermaidRerender();
    }
  }

  // pull window.__LASH_SCENE config from a fetched HTML string (each
  // page's <head> sets a per-page aspect / scale tuple)
  function extractSceneConfig(html) {
    const m = html.match(/window\.__LASH_SCENE\s*=\s*(\{[^}]+\})/);
    if (!m) return null;
    try { return new Function("return " + m[1])(); } catch (e) { return null; }
  }

  async function spaNavigate(url, pushHistory) {
    const targetUrl = new URL(url, location.href);
    // capture the sidebar's internal scroll up-front so we can restore
    // it after the swap — focus/reflow side effects in the middle of
    // the flow can reset it otherwise
    const tocEl = document.querySelector(".toc");
    const savedTocScroll = tocEl ? tocEl.scrollTop : 0;
    try {
      const res = await fetch(targetUrl.href, { credentials: "same-origin" });
      if (!res.ok) throw new Error("bad status");
      const html = await res.text();
      const doc = new DOMParser().parseFromString(html, "text/html");

      // sanity: only SPA-swap if both old and new are docs pages.
      // The .docs-layout shell is BUILT at runtime by mountShell(),
      // so it isn't present in fetched HTML — we have to look at
      // body.docs-page (which IS in the source) plus the per-page
      // pieces we're about to swap. Otherwise fall back to a full
      // navigation so the landing's different structure renders fresh.
      const newIsDocs = doc.body && doc.body.classList.contains("docs-page");
      const curIsDocs = document.body.classList.contains("docs-page");
      const newOpener = doc.querySelector(".opener");
      const newBody   = doc.querySelector(".body");
      if (!newIsDocs || !curIsDocs || !newOpener || !newBody) {
        location.href = targetUrl.href;
        return;
      }

      if (pushHistory) {
        history.pushState({ spa: true }, "", targetUrl.href);
      }

      document.title = doc.title || document.title;
      // body class — guard the docs-page flag (and any future variants)
      document.body.className = doc.body.className;

      // swap opener, body, pager — leave .toc and .cover svg alone
      const swap = (selector) => {
        const fresh = doc.querySelector(selector);
        const here = document.querySelector(selector);
        if (fresh && here) here.replaceWith(fresh.cloneNode(true));
      };
      swap(".opener");
      swap(".body");
      swap(".pager");
      // cover caption: keep the SVG, only swap the source/caption text
      const newCap = doc.querySelector(".cover__caption");
      const curCap = document.querySelector(".cover__caption");
      if (newCap && curCap) curCap.innerHTML = newCap.innerHTML;

      // per-page scene config (aspect ratio etc.)
      const scene = extractSceneConfig(html);
      if (scene) window.__LASH_SCENE = scene;

      // post-swap re-init — TOC active state, per-page outline, spine,
      // mermaid, scene. If the section changed, the sidebar holds the
      // wrong section's groups, so rebuild it; otherwise just refresh
      // active state (cheaper, preserves expanded groups).
      const swapTier = tierForHref(currentPath());
      if (swapTier !== window.__LASH_ACTIVE_TIER) buildTOC();
      else refreshTOCActive();
      syncBandSections(swapTier);
      buildPager();
      mountRelatedLinks();
      injectPageOutline();
      drawSpineSnake();
      loadMermaidIfNeeded();
      runMermaidIfReady();
      if (typeof window.__LASH_HIGHLIGHT === "function") {
        window.__LASH_HIGHLIGHT();
      }
      if (typeof window.__LASH_SCENE_RUN === "function") {
        window.__LASH_SCENE_RUN();
      }

      // scroll: anchor if present, else top of content (sidebar keeps
      // its own scrollTop — that's the point of all this)
      if (targetUrl.hash) {
        const id = decodeURIComponent(targetUrl.hash.slice(1));
        const target = document.getElementById(id);
        if (target) {
          target.scrollIntoView({ block: "start" });
        } else {
          window.scrollTo({ top: 0 });
        }
      } else {
        window.scrollTo({ top: 0 });
      }
      // restore sidebar scroll AFTER everything else — the focus/reflow
      // chain above can reset it back to 0 on Chrome
      if (tocEl) {
        tocEl.scrollTop = savedTocScroll;
        // and again on the next frame in case a deferred layout still
        // wants to undo it
        requestAnimationFrame(() => { tocEl.scrollTop = savedTocScroll; });
      }
    } catch (err) {
      // anything blew up — fall back to a regular navigation
      location.href = targetUrl.href;
    }
  }

  // expose spaNavigate for the cmdk modal (and any future caller that
  // wants to do an SPA swap without simulating a click)
  window.__LASH_SPA = spaNavigate;

  function wireSpaNav() {
    document.addEventListener("click", (ev) => {
      if (ev.defaultPrevented) return;
      if (ev.button !== 0) return;
      if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey) return;
      const a = ev.target.closest && ev.target.closest("a[href]");
      if (!a) return;
      if (a.target && a.target !== "_self") return;
      if (a.hasAttribute("download")) return;
      const href = a.getAttribute("href");
      if (!href || href.startsWith("#")) return;
      const url = new URL(href, location.href);
      if (url.origin !== location.origin) return;
      // only intercept .html pages — leaves mailto:, tel:, raw asset
      // links, and external sites to the browser. The .docs-layout
      // check inside spaNavigate falls back to a full load when the
      // target turns out to be a landing-style page.
      if (!/\.html(?:\?|#|$)/.test(url.pathname)) return;
      ev.preventDefault();
      spaNavigate(url.href, true);
    });

    window.addEventListener("popstate", () => {
      spaNavigate(location.href, false);
    });
  }

  // ── cmdk search — modal triggered by ⌘K / Ctrl+K or the band button ──
  //    Walks the TOC, fetches every page once, extracts opener title +
  //    section h2/h3 headings, and offers a substring-matched dropdown.
  //    Selecting a result hands off to spaNavigate when possible.
  const SEARCH = {
    overlay: null,
    input: null,
    list: null,
    status: null,
    results: [],
    selected: 0,
    indexed: false,
    indexing: false,
  };

  function searchAllPages() {
    // flatten TOC into the unique page list, preserving order
    const seen = new Set();
    const pages = [];
    for (const { title, name, href, summary, kind, track } of flatTOC()) {
      if (!seen.has(href)) {
        seen.add(href);
        pages.push({ href, label: title || name, summary, kind, track });
      }
    }
    return pages;
  }

  async function buildSearchIndex() {
    // Only treat a NON-EMPTY index as a valid cache. An empty array is
    // truthy, so caching it (e.g. after a transient fetch failure) would
    // wedge ⌘K on "no matches" for the rest of the session — reopening
    // search must be free to retry the build.
    if (Array.isArray(window.__LASH_SEARCH_INDEX) && window.__LASH_SEARCH_INDEX.length) {
      SEARCH.indexed = true;
      return window.__LASH_SEARCH_INDEX;
    }
    if (SEARCH.indexing) return null;
    SEARCH.indexing = true;
    const base = tocBase();
    const pages = searchAllPages();
    const entries = [];
    try {
      const docs = await Promise.all(pages.map((p) =>
        fetch(base + p.href, { credentials: "same-origin" })
          .then((r) => (r.ok ? r.text() : ""))
          .catch(() => "")
      ));
      pages.forEach((p, idx) => {
        const html = docs[idx];
        if (!html) return;
        const doc = new DOMParser().parseFromString(html, "text/html");
        const titleEl = doc.querySelector(".opener__title");
        const pageTitle = (titleEl ? titleEl.textContent : p.label || p.href)
          .replace(/\s+/g, " ").trim();
        entries.push({
          url: p.href,
          page_title: pageTitle,
          heading_text: "",
          summary: p.summary || "",
          track: p.track || "",
          kind: "page",
        });
        const body = doc.querySelector(".body");
        if (!body) return;
        const headings = body.querySelectorAll("h2, h3");
        headings.forEach((h) => {
          const text = (h.textContent || "").replace(/\s+/g, " ").trim();
          if (!text) return;
          // find the nearest ancestor with an id (.section / .stanza / etc.)
          let anchor = "";
          let node = h;
          while (node && node !== body) {
            if (node.id) { anchor = node.id; break; }
            node = node.parentElement;
          }
          entries.push({
            url: anchor ? p.href + "#" + anchor : p.href,
            page_title: pageTitle,
            heading_text: text,
            summary: p.summary || "",
            track: p.track || "",
            kind: "heading",
          });
        });
      });
    } finally {
      SEARCH.indexing = false;
    }
    // Cache only a real index; leave a failed/empty build uncached so
    // the next openSearch retries instead of serving "no matches".
    if (entries.length) {
      window.__LASH_SEARCH_INDEX = entries;
      SEARCH.indexed = true;
    }
    return entries;
  }

  function filterSearch(query) {
    const entries = window.__LASH_SEARCH_INDEX || [];
    const q = query.trim().toLowerCase();
    if (!q) return entries.slice(0, 30);
    const matches = [];
    for (const e of entries) {
      const hay = (e.page_title + " " + e.heading_text + " " + (e.summary || "") + " " + (e.track || "")).toLowerCase();
      if (hay.indexOf(q) < 0) continue;
      // page-title hits rank above heading hits
      const titleHit = e.page_title.toLowerCase().indexOf(q) >= 0;
      const rank = (e.kind === "page" ? 0 : 1) + (titleHit ? 0 : 2);
      matches.push({ e, rank });
    }
    matches.sort((a, b) => a.rank - b.rank);
    return matches.slice(0, 30).map((m) => m.e);
  }

  function renderSearchResults() {
    if (!SEARCH.list) return;
    if (!SEARCH.indexed && SEARCH.indexing) {
      SEARCH.list.innerHTML = '<li class="cmdk__empty">indexing…</li>';
      return;
    }
    if (!SEARCH.results.length) {
      SEARCH.list.innerHTML = '<li class="cmdk__empty">no matches</li>';
      return;
    }
    const base = tocBase();
    const parts = [];
    SEARCH.results.forEach((r, i) => {
      const cls = "cmdk__row" + (i === SEARCH.selected ? " is-active" : "");
      const kindLabel = r.kind === "page" ? "page" : "section";
      const headingHtml = r.heading_text
        ? `<span class="cmdk__row-heading">${escapeHtml(r.heading_text)}</span>`
        : "";
      parts.push(
        `<li><a class="${cls}" data-idx="${i}" href="${escapeHtml(base + r.url)}">` +
          `<span class="cmdk__row-kind">${kindLabel}</span>` +
          `<span class="cmdk__row-body">` +
            `<span class="cmdk__row-title">${escapeHtml(r.page_title)}</span>` +
            headingHtml +
          `</span>` +
        `</a></li>`
      );
    });
    SEARCH.list.innerHTML = parts.join("");
  }

  function ensureSearchActiveVisible() {
    if (!SEARCH.list) return;
    const active = SEARCH.list.querySelector(".cmdk__row.is-active");
    if (active && typeof active.scrollIntoView === "function") {
      active.scrollIntoView({ block: "nearest" });
    }
  }

  function openSearch() {
    if (!SEARCH.overlay) mountSearchModal();
    if (SEARCH.overlay.classList.contains("is-open")) return;
    SEARCH.overlay.classList.add("is-open");
    document.documentElement.classList.add("cmdk-open");
    SEARCH.input.value = "";
    SEARCH.selected = 0;
    SEARCH.results = [];
    // kick off the index fetch if it hasn't been built yet
    if (!window.__LASH_SEARCH_INDEX) {
      renderSearchResults(); // show "indexing…" state
      buildSearchIndex().then(() => {
        if (!SEARCH.overlay.classList.contains("is-open")) return;
        SEARCH.results = filterSearch(SEARCH.input.value);
        SEARCH.selected = 0;
        renderSearchResults();
      });
    } else {
      SEARCH.results = filterSearch("");
      renderSearchResults();
    }
    // focus on next frame — focus() inside a click that opened us can
    // race against the click's own focus management
    requestAnimationFrame(() => { SEARCH.input.focus(); });
  }

  function closeSearch() {
    if (!SEARCH.overlay) return;
    SEARCH.overlay.classList.remove("is-open");
    document.documentElement.classList.remove("cmdk-open");
  }

  function activateSearchResult(idx) {
    const r = SEARCH.results[idx];
    if (!r) return;
    const base = tocBase();
    const url = base + r.url;
    closeSearch();
    if (typeof window.__LASH_SPA === "function") {
      window.__LASH_SPA(url, true);
    } else {
      location.href = url;
    }
  }

  function mountSearchModal() {
    const overlay = document.createElement("div");
    overlay.className = "cmdk";
    overlay.setAttribute("aria-hidden", "true");
    overlay.innerHTML =
      '<div class="cmdk__dimmer" data-cmdk-dismiss></div>' +
      '<div class="cmdk__card" role="dialog" aria-label="search docs">' +
        '<div class="cmdk__search-row">' +
          '<span class="cmdk__icon" aria-hidden="true">⌕</span>' +
          '<input class="cmdk__input" type="text" autocomplete="off" ' +
            'spellcheck="false" placeholder="search the docs…" aria-label="search">' +
          '<kbd class="cmdk__esc">esc</kbd>' +
        '</div>' +
        '<ul class="cmdk__list" role="listbox"></ul>' +
        '<div class="cmdk__hint">' +
          '<span><kbd>↑</kbd><kbd>↓</kbd> navigate</span>' +
          '<span><kbd>↵</kbd> open</span>' +
          '<span><kbd>esc</kbd> close</span>' +
        '</div>' +
      '</div>';
    document.body.appendChild(overlay);
    SEARCH.overlay = overlay;
    SEARCH.input = overlay.querySelector(".cmdk__input");
    SEARCH.list = overlay.querySelector(".cmdk__list");

    SEARCH.input.addEventListener("input", () => {
      if (!window.__LASH_SEARCH_INDEX) {
        renderSearchResults();
        return;
      }
      SEARCH.results = filterSearch(SEARCH.input.value);
      SEARCH.selected = 0;
      renderSearchResults();
    });
    SEARCH.input.addEventListener("keydown", (ev) => {
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        if (SEARCH.results.length === 0) return;
        SEARCH.selected = (SEARCH.selected + 1) % SEARCH.results.length;
        renderSearchResults();
        ensureSearchActiveVisible();
      } else if (ev.key === "ArrowUp") {
        ev.preventDefault();
        if (SEARCH.results.length === 0) return;
        SEARCH.selected = (SEARCH.selected - 1 + SEARCH.results.length) % SEARCH.results.length;
        renderSearchResults();
        ensureSearchActiveVisible();
      } else if (ev.key === "Enter") {
        ev.preventDefault();
        activateSearchResult(SEARCH.selected);
      } else if (ev.key === "Escape") {
        ev.preventDefault();
        closeSearch();
      }
    });
    overlay.addEventListener("click", (ev) => {
      if (ev.target && ev.target.hasAttribute && ev.target.hasAttribute("data-cmdk-dismiss")) {
        closeSearch();
        return;
      }
      const row = ev.target.closest && ev.target.closest(".cmdk__row");
      if (!row) return;
      ev.preventDefault();
      const idx = parseInt(row.getAttribute("data-idx"), 10);
      if (Number.isFinite(idx)) activateSearchResult(idx);
    });
    overlay.addEventListener("mousemove", (ev) => {
      const row = ev.target.closest && ev.target.closest(".cmdk__row");
      if (!row) return;
      const idx = parseInt(row.getAttribute("data-idx"), 10);
      if (!Number.isFinite(idx) || idx === SEARCH.selected) return;
      SEARCH.selected = idx;
      renderSearchResults();
    });
  }

  function wireSearchTrigger() {
    // global keydown — install once at boot; persists across spaNavigate
    document.addEventListener("keydown", (ev) => {
      const k = ev.key;
      if ((ev.metaKey || ev.ctrlKey) && (k === "k" || k === "K")) {
        ev.preventDefault();
        if (SEARCH.overlay && SEARCH.overlay.classList.contains("is-open")) {
          closeSearch();
        } else {
          openSearch();
        }
      }
    });
    // band-level click handler — the trigger button uses
    // data-cmdk-open so it survives any SPA swap that re-emits the
    // band markup
    document.addEventListener("click", (ev) => {
      const btn = ev.target.closest && ev.target.closest("[data-cmdk-open]");
      if (!btn) return;
      ev.preventDefault();
      openSearch();
    });
  }

  // ── shell mount ───────────────────────────────────────────
  // Each docs page used to ship its own copy of the band, layout
  // wrapper, toc aside, pager, and cover SVG. That's now built here so
  // the per-page HTML stays minimal. Legacy pages that still carry the
  // shell are detected and skipped.
  const GITHUB_ICON =
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true">' +
    '<path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2 .37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z"/></svg>';

  // platform-aware label for the search shortcut chip — macOS sees
  // ⌘K, everyone else sees Ctrl+K (the symbol means nothing off-mac)
  function searchShortcutLabel() {
    const plat = (navigator.userAgentData && navigator.userAgentData.platform)
      || navigator.platform
      || "";
    return /mac|iphone|ipad|ipod/i.test(plat) ? "⌘K" : "Ctrl+K";
  }

  // The cover caption ships per-page as a data-attribute. The value is
  // a list of <code>path/to/file</code> spans joined by &middot;. Wrap
  // each path in a GH source link so the caption is navigable, not
  // inert prose.
  function linkifyCoverSource(html, repoBase) {
    if (!html) return "";
    return html.replace(
      /<code>([^<]+)<\/code>/g,
      (_, path) => {
        const url = repoBase + "/blob/main/" + path;
        return `<a class="cover__src" href="${url}" target="_blank" rel="noopener"><code>${path}</code></a>`;
      }
    );
  }

  function mountShell() {
    if (!document.body.classList.contains("docs-page")) return;
    if (document.querySelector(".docs-layout")) return; // legacy page; skip

    const base = tocBase();
    const body = document.body;
    const repoBase = "https://github.com/SamGalanakis/lash";

    // pull out the per-page pieces (opener, body, optional pager)
    const opener = body.querySelector(":scope > .opener");
    const main = body.querySelector(":scope > main.body, :scope > .body");
    let pager = body.querySelector(":scope > .pager");
    const coverSource = body.dataset.coverSource || "";

    if (!opener || !main) return; // shape doesn't match; bail

    // ── build skip-link ─────────────────────────────────────
    const skip = document.createElement("a");
    skip.className = "skip-link";
    skip.href = "#content";
    skip.textContent = "skip to content";

    // ── build band ──────────────────────────────────────────
    const kb = searchShortcutLabel();
    const kbAria = kb === "⌘K" ? "cmd + k" : "ctrl + k";
    const band = document.createElement("header");
    band.className = "band";
    band.innerHTML =
      `<span class="band__brand"><a href="${base}index.html">lash</a></span>` +
      bandSectionsHTML(base) +
      '<div class="band__right">' +
        `<button class="band__btn band__btn--search" type="button" data-cmdk-open ` +
          `aria-label="search docs (${kbAria})" title="search docs (${kbAria})">` +
          '<span class="cmdk-label">search</span>' +
          `<span class="cmdk-kbd" aria-hidden="true">${kb}</span>` +
        '</button>' +
        `<a class="band__btn" href="${base}architecture/reference.html" ` +
          `aria-label="api reference" title="api reference (rustdoc on docs.rs)">` +
          '<span>api</span><span aria-hidden="true">↗</span>' +
        '</a>' +
        `<a class="band__btn band__btn--icon" href="https://github.com/SamGalanakis/lash" ` +
          `target="_blank" rel="noopener" aria-label="lash on GitHub" title="lash on GitHub">${GITHUB_ICON}</a>` +
        '<button class="band__btn" id="theme-toggle" type="button" ' +
          'aria-label="toggle theme" title="toggle light / dark">' +
          '<span class="glyph" id="theme-glyph" aria-hidden="true">◐</span>' +
          '<span id="theme-label">light</span>' +
        '</button>' +
      '</div>';

    // ── build docs-layout shell ─────────────────────────────
    const layout = document.createElement("div");
    layout.className = "docs-layout";
    const toc = document.createElement("aside");
    toc.className = "toc";
    toc.setAttribute("aria-label", "documentation contents");
    const mainCol = document.createElement("div");
    mainCol.className = "docs-main";
    layout.appendChild(toc);
    layout.appendChild(mainCol);

    // ensure a pager exists (auto-filled later by buildPager)
    if (!pager) {
      pager = document.createElement("section");
      pager.className = "pager";
      pager.setAttribute("aria-label", "next chapter");
    }

    // ── build cover ─────────────────────────────────────────
    const cover = document.createElement("section");
    cover.className = "cover";
    cover.setAttribute("aria-label", "footer · twilight landscape");
    cover.innerHTML =
      '<svg id="range" class="cover__svg" preserveAspectRatio="xMidYMid slice" ' +
        'xmlns="http://www.w3.org/2000/svg" role="img" ' +
        'aria-label="A warm-iron twilight landscape across the full width of the page.">' +
        '<defs><linearGradient id="skyGrad" x1="0" y1="0" x2="0" y2="1">' +
          '<stop offset="0%" stop-color="#141412"/>' +
          '<stop offset="65%" stop-color="#1a1814"/>' +
          '<stop offset="100%" stop-color="#221d16"/>' +
        '</linearGradient></defs>' +
        '<rect id="sky-rect" width="100%" height="100%" fill="transparent"/>' +
        '<g id="g-back"></g>' +
        '<g id="g-mid"></g>' +
        '<g id="g-lash-behind"></g>' +
        '<g id="g-near"></g>' +
        '<g id="g-lash-front"></g>' +
        '<g id="g-ground"></g>' +
      '</svg>' +
      '<p class="cover__caption">' +
        (coverSource
          ? `<span>source &middot; ${linkifyCoverSource(coverSource, repoBase)}</span>`
          : '<span>&nbsp;</span>') +
        '<a class="cover__top" href="#top">back to top <span class="arrow" aria-hidden="true">↑</span></a>' +
      '</p>';

    // ── move pieces into the shell ──────────────────────────
    mainCol.appendChild(opener);
    mainCol.appendChild(main);
    mainCol.appendChild(pager);

    // clear body and re-populate in the right order
    body.id = body.id || "top";
    // remove anything else stranded at body root (data nodes etc.)
    [...body.children].forEach(c => c.remove());
    body.appendChild(skip);
    body.appendChild(band);
    body.appendChild(layout);
    body.appendChild(cover);

    // load the scene script if it isn't already on the page
    if (!document.querySelector('script[data-scene]')) {
      const s = document.createElement("script");
      s.src = `${base}scene.js?v=${SCENE_ASSET_VERSION}`;
      s.defer = true;
      s.setAttribute("data-scene", "");
      document.head.appendChild(s);
    }
  }

  // On landing pages the band ships in static HTML; update the
  // hard-coded ⌘K chip so non-mac users see Ctrl+K too.
  function fixupSearchChip() {
    const kb = document.querySelector(".cmdk-kbd");
    if (kb) kb.textContent = searchShortcutLabel();
  }

  function bootstrap() {
    mountShell();
    fixupSearchChip();
    mountLandingRegistry();
    mountRelatedLinks();
    buildTOC();
    buildPager();
    autoKnot();
    injectPageOutline();
    drawSpineSnake();
    mountTocJump();
    loadMermaidIfNeeded();
    wireSpaNav();
    wireSearchTrigger();
    // Re-run the scene only on docs pages — the brazier strand needs
    // to converge at the snake's bottom (now that drawSpineSnake has
    // set __LASH_SNAKE_ANCHOR_X). On the landing there's no spine
    // anchor to update, and the second run would just cause a visible
    // double-animation right after the IIFE's first one.
    if (document.body.classList.contains("docs-page") &&
        typeof window.__LASH_SCENE_RUN === "function") {
      window.__LASH_SCENE_RUN();
    }
  }

  // redraw the snake (and re-converge the strand) on resize
  let resizeTimer = null;
  window.addEventListener("resize", () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      drawSpineSnake();
      if (typeof window.__LASH_SCENE_RUN === "function") {
        window.__LASH_SCENE_RUN();
      }
    }, 200);
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootstrap);
  } else {
    bootstrap();
  }
})();
