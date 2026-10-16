export const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);
// Lash's view is absent when the document does not link; it then has nothing.
const NO_VIEW = {
  statements: [],
  nodes: [], callables: [],
  scopes: [],
  dataflow: {
    bindings: [],
    expressions: [],
    actions: [],
    parameters: [],
    returns: [],
    assignments: [],
  },
  execution_sites: [],
};
export const viewOf = (workflow) => workflow.view ?? NO_VIEW;
// An action carrier is keyed by its exact action Site. Statement payloads
// remain context; they never select a representative action for a tile.
export const actionOf = (workflow, carrier) =>
  viewOf(workflow).dataflow.actions.find((action) => siteKey(action.site) === siteKey(carrier.site));
// The effect a statement's node performs, if its action is a `perform`.
export function effectOf(workflow, s) {
  const action = actionOf(workflow, s);
  return action?.kind === "perform" ? action.callee?.effect : undefined;
}
export function statementsIn(workflow, block) {
  return viewOf(workflow).statements.filter(
    (s) => siteKey(s.block) === siteKey(block),
  );
}
// How this host words a statement in its list. Lash's view carries no wording.
function valueSummary(rhs) {
  const action = rhs?.action ?? rhs?.expr?.act?.kind;
  if (!action) return "a value";
  if (action.perform) return `perform ${action.perform.effect}`;
  const kind = typeof action === "string" ? action : Object.keys(action)[0];
  return kind === "join_many" ? "join" : kind;
}
export function summary(statement) {
  if (typeof statement === "string") return statement;
  const [kind, body] = Object.entries(statement)[0];
  if (kind === "let") return `let ${body.name} = ${valueSummary(body.value)}`;
  if (kind === "assign")
    return `set ${body.place.variable ?? "a member"} = ${valueSummary(body.value)}`;
  if (kind === "remove") return "remove a member";
  if (kind === "do") return valueSummary({ action: body.action });
  if (kind === "for") return `for ${body.binding}`;
  return kind;
}
export function startPosition(workflow, block) {
  const first = statementsIn(workflow, block)[0];
  return { block, ...(first ? { before: first.site } : {}) };
}
function terminal(statement) {
  const kind =
    typeof statement === "string" ? statement : Object.keys(statement)[0];
  return ["return", "throw", "finish", "fail", "break", "continue"].includes(
    kind,
  );
}
export function appendPosition(workflow, block) {
  const end = statementsIn(workflow, block).find((s) => terminal(s.statement));
  return { block, ...(end ? { before: end.site } : {}) };
}
function references(value, name) {
  if (!value || typeof value !== "object") return 0;
  return (
    Number(value.variable === name) +
    Object.values(value).reduce((n, child) => n + references(child, name), 0)
  );
}
// Only a single-use, adjacent record binding is safe for this small form editor.
// Other inputs remain canonical expressions available through Advanced.
export function simpleBinding(workflow, s, value) {
  if (!value?.variable) return;
  const siblings = statementsIn(workflow, s.block);
  const previous =
    siblings[
      siblings.findIndex((t) => siteKey(t.site) === siteKey(s.site)) - 1
    ];
  if (previous?.statement.let?.name !== value.variable) return;
  if (references(workflow.document, value.variable) !== 1) return;
  return previous;
}
// The record an effect step takes as its first argument, written in place
// as the argument, with the site an edit replaces it at.
export function recordInput(workflow, s) {
  const action = actionOf(workflow, s);
  if (action?.kind !== "perform") return;
  const argument = action.args[0];
  const record = argument?.expression?.record;
  return record && argument.site ? { site: argument.site, record } : undefined;
}
export function literal(value) {
  if (value?.literal && typeof value.literal === "object")
    return Object.values(value.literal)[0];
}
export function conditionValue(workflow, s) {
  const condition = s.statement.if?.condition;
  return (
    literal(condition) ??
    literal(simpleBinding(workflow, s, condition)?.statement.let.value.expr)
  );
}
