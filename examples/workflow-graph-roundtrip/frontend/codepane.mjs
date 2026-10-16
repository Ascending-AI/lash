// The code pane: light syntax colouring, and the statement each line shows
// where lash gives one. Nothing here parses a language: TypeScript lines
// come from the printer's source map, kernel lines from the kernel printer's
// layout, checked statement by statement.

const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);

// The keyword the kernel printer starts each statement with.
const KERNEL_KEYWORDS = {
  let: "let",
  assign: "set",
  remove: "remove",
  do: "do",
  if: "if",
  for: "for",
  while: "while",
  try: "try",
  return: "return",
  throw: "throw",
  print: "print",
  finish: "finish",
  fail: "fail",
  break: "break",
  continue: "continue",
};

const tagOf = (statement) =>
  typeof statement === "string" ? statement : Object.keys(statement)[0];

// A statement ↔ line correspondence: `lines(site)` is the first and last
// line (0-based) a statement spans, `siteAt(line)` the statement a click on
// `line` means, and `step(key, delta)` the statement an arrow key moves to.
function linking(spans) {
  const bySite = new Map(spans.map((span) => [span.key, span]));
  const ordered = [...spans].sort((a, b) => a.start - b.start);
  return {
    lines: (site) => bySite.get(siteKey(site)),
    // The statement starting on the next (`delta` 1) or previous (-1)
    // statement line; from an unlinked key, the first or last.
    step(key, delta) {
      const at = ordered.findIndex((span) => span.key === key);
      const next =
        at < 0 ? (delta > 0 ? 0 : ordered.length - 1) : at + delta;
      return ordered[next]?.key;
    },
    siteAt(line) {
      // The innermost statement starting on the line, else the innermost
      // one spanning it.
      let starting, spanning;
      for (const span of spans) {
        if (span.start === line) starting = span;
        if (span.start <= line && line <= span.end) spanning = span;
      }
      return (starting ?? spanning)?.key;
    },
  };
}

// The kernel printer writes every statement on a line of its own, in
// document order, and only `}` begins a line that closes or continues a
// block. Each statement of the entry's view is matched to the next such
// line of the entry's function and checked against its keyword; any
// mismatch means no linking.
export function kernelLinking(text, view, entry) {
  const lines = text.split("\n");
  const header = lines.findIndex((line) => line.startsWith(`fn ${entry}(`));
  if (header < 0) return undefined;
  const statements = view.statements;
  const spans = [];
  let next = 0;
  for (let line = header + 1; line < lines.length; line++) {
    if (lines[line] === "}") break;
    const code = lines[line].trimStart();
    if (!code || code.startsWith("}")) continue;
    const statement = statements[next++];
    if (!statement) return undefined;
    const keyword = KERNEL_KEYWORDS[tagOf(statement.statement)];
    if (!keyword || (code !== keyword && !code.startsWith(keyword + " ")))
      return undefined;
    spans.push({
      key: siteKey(statement.site),
      start: line,
      end: blockEnd(lines, line),
    });
  }
  return next === statements.length ? linking(spans) : undefined;
}

const indent = (line) => line.length - line.trimStart().length;
// The last line of the statement starting at `start`: its nested lines are
// indented deeper, and a `}` at its own indent closes or continues it.
function blockEnd(lines, start) {
  const depth = indent(lines[start]);
  let end = start;
  for (let line = start + 1; line < lines.length; line++) {
    const deeper = indent(lines[line]) > depth;
    const closing =
      indent(lines[line]) === depth && lines[line].trimStart().startsWith("}");
    if (!deeper && !closing) break;
    end = line;
  }
  return end;
}

// Lash's TypeScript source map gives byte ranges of the printed source for
// each site; a statement spans the lines of its first range.
export function sourceMapLinking(source, sourceMap, view) {
  if (!sourceMap) return undefined;
  const encoder = new TextEncoder();
  const starts = [0];
  for (const line of source.split("\n"))
    starts.push(starts.at(-1) + encoder.encode(line).length + 1);
  const lineOf = (offset) => {
    let low = 0,
      high = starts.length - 2;
    while (low < high) {
      const middle = (low + high + 1) >> 1;
      if (starts[middle] <= offset) low = middle;
      else high = middle - 1;
    }
    return low;
  };
  const ranges = new Map(
    sourceMap.sites.map((entry) => [siteKey(entry.site), entry.ranges[0]]),
  );
  const spans = [];
  for (const statement of view.statements) {
    const range = ranges.get(siteKey(statement.site));
    if (!range) continue;
    spans.push({
      key: siteKey(statement.site),
      start: lineOf(range.start),
      end: lineOf(Math.max(range.start, range.end - 1)),
    });
  }
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
