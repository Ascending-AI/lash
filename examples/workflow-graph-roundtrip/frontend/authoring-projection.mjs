export const siteKey = (site) => JSON.stringify([site.unit, site.path ?? []]);
export function actionOf(s) {
  const v = s.statement;
  return v.let?.value?.action ?? v.assign?.value?.action ?? v.do?.action;
}
export function statementsIn(workflow, block) {
  return workflow.statements.filter((s) => siteKey(s.block) === siteKey(block));
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
export function project(workflow, block, describe) {
  const nodes = [];
  for (const s of statementsIn(workflow, block)) {
    if (s.statement.if) {
      const branch = (index) =>
        project(
          workflow,
          { unit: s.site.unit, path: [...(s.site.path ?? []), index] },
          describe,
        );
      nodes.push({
        ...describe(s),
        branch: true,
        children: [branch(1), branch(2)],
      });
    } else if (
      actionOf(s) ||
      s.statement.try ||
      s.statement.for ||
      s.statement.while
    ) {
      nodes.push(describe(s));
    }
    if (terminal(s.statement)) break;
  }
  return nodes;
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
export function recordInput(workflow, s) {
  const binding = simpleBinding(workflow, s, actionOf(s)?.perform?.args?.[0]);
  const record = binding?.statement.let.value.expr?.record;
  return record ? { statement: binding, record } : undefined;
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
