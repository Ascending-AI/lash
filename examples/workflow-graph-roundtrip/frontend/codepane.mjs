// Syntax colouring and canonical-node selection from printer line ranges.
// The browser neither parses source nor reconstructs statement boundaries.

const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);

// A node ↔ line correspondence: `lines(site)` is the first and last
// line (0-based) a node spans, `siteAt(line)` the node a click on
// `line` means, and `step(key, delta)` the node an arrow key moves to.
function linking(spans) {
  const bySite = new Map(spans.map((span) => [span.key, span]));
  const ordered = [...spans].sort((a, b) => a.start - b.start);
  return {
    lines: (site) => bySite.get(siteKey(site)),
    // The node starting on the next (`delta` 1) or previous (-1)
    // node line; from an unlinked key, the first or last.
    step(key, delta) {
      const at = ordered.findIndex((span) => span.key === key);
      const next =
        at < 0 ? (delta > 0 ? 0 : ordered.length - 1) : at + delta;
      return ordered[next]?.key;
    },
    siteAt(line) {
      // A carrier starting on this line, else one spanning it. Keyboard
      // navigation retains every carrier, including shared-line actions.
      let starting, spanning;
      for (const span of spans) {
        if (span.start === line) starting = span;
        if (span.start <= line && line <= span.end) spanning = span;
      }
      return (starting ?? spanning)?.key;
    },
  };
}

// Both lenses use ranges supplied by the view. Canvas membership and
// canonical locations survive a readable printer's refusal.
export function kernelLinking(text, view) {
  if (text !== view.canonical_source) return undefined;
  return linking(view.nodes.map((node) => ({ key: siteKey(node.site),
    start: node.canonical.first - 1, end: node.canonical.last - 1 })));
}
export function readableLinking(view) {
  if (view.readable?.kind !== "available") return undefined;
  const spans = view.nodes.filter((node) => node.readable.length).map((node) => ({
    key: siteKey(node.site), start: Math.min(...node.readable.map((range) => range.first)) - 1,
    end: Math.max(...node.readable.map((range) => range.last)) - 1,
  }));
  return spans.length ? linking(spans) : undefined;
}

const KEYWORDS = {
  typescript: new Set(
    "as async await break case catch class const continue default do else export for function if in let new null of return switch this throw true false try type typeof undefined var while yield".split(
      " ",
    ),
  ),
  python: new Set(
    "and as async await break class continue def elif else except False finally for from if import in is lambda None not or pass raise return True try while with".split(
      " ",
    ),
  ),
  kernel: new Set(
    "absent as break catch continue do effect else entry fail false finally finish fn for if in invoke join kernel let main null numbers perform print private remove return set sleep spawn throw true try use while yield".split(
      " ",
    ),
  ),
};
const TOKEN =
  /(\/\/.*$)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)|(\b\d+(?:\.\d+)?\b)|([A-Za-z_$][\w$]*)/g;
const escape = (text) =>
  text.replace(
    /[&<>]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c],
  );

// One line as HTML: keywords, strings, numbers and comments in spans.
// Strings spanning lines are rare in printed code and are left plain.
export function highlightLine(line, language) {
  const keywords = KEYWORDS[language] ?? new Set();
  let html = "",
    last = 0;
  for (const match of line.matchAll(TOKEN)) {
    html += escape(line.slice(last, match.index));
    const [token, comment, string, number, word] = match;
    const kind = comment
      ? "comment"
      : string
        ? "string"
        : number
          ? "number"
          : keywords.has(word)
            ? "keyword"
            : undefined;
    html += kind
      ? `<span class="tok-${kind}">${escape(token)}</span>`
      : escape(token);
    last = match.index + token.length;
  }
  return html + escape(line.slice(last));
}
