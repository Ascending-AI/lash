// Captions on the canvas. Lash joins the version's captions with its view
// (`captionView`, the kernel-caption-view shape); this host only looks them
// up by view location, words a step that has none, and builds the entry
// list a rename saves. The backend turns that list into a Captions value.
import { siteKey } from "./projection.mjs";

const NO_CAPTIONS = { nodes: [], arms: [], callables: [], params: [], fields: [] };
const joined = (workflow) => workflow?.captionView ?? NO_CAPTIONS;
const reviewing = (workflow, site) =>
  (workflow?.captionsToReview ?? []).some((item) => siteKey(item) === siteKey(site));

// Every caption lash draws on the view node at `index`: an action or
// control node's carrier statement's, or each captioned member of a code
// node. Each carries whether an edit has marked it for review.
export function nodeCaptions(workflow, index) {
  return joined(workflow)
    .nodes.filter((item) => item.node === index)
    .map((item) => ({ site: item.site, caption: item.caption, review: reviewing(workflow, item.site) }));
}

// The caption of arm `arm` of the control node at `index`, if it has one.
export function armCaption(workflow, index, arm) {
  return joined(workflow).arms.find((item) => item.node === index && item.arm === arm)?.caption;
}

// The workflow's title: the caption of its entry function, if it has one.
export function workflowTitle(workflow) {
  const root = siteKey({ unit: { function: workflow?.entry }, path: [] });
  return joined(workflow).callables.find((item) => siteKey(item.block) === root)?.caption;
}

// Where a caption for a view node goes when it has none yet. An action or
// control node is captioned on its statement, and only the node holding the
// statement's first action may carry it; a code node on its first member.
export function captionSite(node) {
  if (node.body.kind === "code") return node.body.members[0];
  return node.body.members.some((member) => siteKey(member) === siteKey(node.statement))
    ? node.statement
    : undefined;
}

// The whole entry list with `site` captioned `caption`, or uncaptioned when
// `caption` is undefined. Every other entry, a parameter's or a field's too,
// is kept as it is.
export function withCaption(captions, site, caption) {
  const kept = (captions?.entries ?? []).filter(
    (entry) => !entry.site || siteKey(entry.site) !== siteKey(site),
  );
  return caption ? [...kept, { site, caption }] : kept;
}

// A caption from form text: the title trimmed, and a description only when
// one is written. Lash checks the rest of the text rule when it is saved.
export function captionFrom(title, description) {
  const text = description.trim();
  return { title: title.trim(), ...(text ? { description: text } : {}) };
}

// The condition a printed `if` or `while` line tests, for a condition step
// with no caption: `if (value < 0) {` and `if value < 0:` are `value < 0`.
export function conditionText(line) {
  const test = line
    ?.trim()
    .replace(/^(if|while)\b\s*/, "")
    .replace(/\s*[{:]$/, "");
  if (!test) return undefined;
  return wrapped(test) ? test.slice(1, -1) : test;
}

// Whether one pair of parentheses encloses all of `text`.
function wrapped(text) {
  if (!text.startsWith("(")) return false;
  let depth = 0;
  for (const [index, char] of [...text].entries()) {
    depth += char === "(" ? 1 : char === ")" ? -1 : 0;
    if (depth === 0) return index === [...text].length - 1;
  }
  return false;
}
