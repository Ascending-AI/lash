import { siteKey, statementsIn, viewOf } from "./projection.mjs";

export function typeName(type) {
  if (!type) return "Any";
  if (typeof type === "string")
    return (
      {
        text: "Text",
        bool: "Boolean",
        int: "Integer",
        float: "Float",
        number: "Number",
        any: "Any",
        null: "Null",
        absent: "Absent",
      }[type] ?? type
    );
  if (type.list) return `List<${typeName(type.list)}>`;
  if (type.record) return "Record";
  if (type.enum) return type.enum.map((v) => JSON.stringify(v)).join(" | ");
  if (type.union) return type.union.map(typeName).join(" | ");
  if (type.function) return "Function";
  if (type.task) return `Task<${typeName(type.task)}>`;
  return Object.keys(type)[0] ?? "Any";
}
export function constantValue(expr) {
  if (expr?.literal === "null") return { known: true, value: null };
  const literal = expr?.literal;
  if (literal && typeof literal === "object") {
    const [kind, value] = Object.entries(literal)[0];
    if (["text", "bool"].includes(kind)) return { known: true, value };
    if (kind === "float" && Number.isFinite(Number(value)))
      return { known: true, value: Number(value) };
    if (kind === "int" && Number.isSafeInteger(Number(value)))
      return { known: true, value: Number(value) };
  }
  if (expr?.record) {
    const values = expr.record.map(({ field, value }) => [
      field,
      constantValue(value),
    ]);
    if (values.every(([, v]) => v.known))
      return {
        known: true,
        value: Object.fromEntries(values.map(([k, v]) => [k, v.value])),
      };
  }
  if (expr?.list) {
    const values = expr.list.map(constantValue);
    if (values.every((v) => v.known))
      return { known: true, value: values.map((v) => v.value) };
  }
  return { known: false };
}
export function constantText(value, depth = 0) {
  if (Object.is(value, -0)) return "-0";
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  const array = Array.isArray(value);
  const entries = array
    ? value.map((item) => constantText(item, depth + 1))
    : Object.entries(value).map(
        ([key, item]) =>
          `${JSON.stringify(key)}: ${constantText(item, depth + 1)}`,
      );
  const [open, close] = array ? ["[", "]"] : ["{", "}"];
  if (!entries.length) return open + close;
  return `${open}\n${entries.map((entry) => "  ".repeat(depth + 1) + entry).join(",\n")}\n${"  ".repeat(depth)}${close}`;
}
export function expressionName(expr) {
  const literal = constantValue(expr);
  if (literal.known) return JSON.stringify(literal.value);
  if (expr?.variable) return expr.variable;
  const field = expr?.member?.field;
  if (field) return `${expressionName(field.target)}.${field.field}`;
  const index = expr?.member?.index;
  if (index)
    return `${expressionName(index.target)}[${expressionName(index.index)}]`;
  if (expr?.record) return "Record";
  if (expr?.closure) return "Function";
  return "Expression";
}
export function fieldsOf(type) {
  return type?.record?.fields ?? [];
}
export function buildDataflow(workflow, block) {
  const view = viewOf(workflow);
  const data = view.dataflow;
  const bindings = new Map(data.bindings.map((b) => [b.id, b]));
  const expressions = new Map(
    data.expressions.map((e) => [siteKey(e.site), e]),
  );
  const rows = new Map(view.statements.map((s) => [siteKey(s.site), s]));
  function expressionPort(name, type, expr) {
    return {
      id: siteKey(expr.site),
      name,
      type: type ?? expr.type,
      expression: expr.expression,
      target: { expression: expr.site },
      context: expr,
      references: expr.references,
      available: expr.available,
    };
  }
  function recordPorts(expr, expected) {
    if (!expr.expression.record.length)
      return [expressionPort("value", expected, expr)];
    return expr.expression.record
      .map((field, index) => {
        const child = expressions.get(
          siteKey({ ...expr.site, path: [...(expr.site.path ?? []), index] }),
        );
        if (!child) return undefined;
        return expressionPort(
          field.field,
          fieldsOf(expected).find((f) => f.name === field.field)?.ty,
          child,
        );
      })
      .filter(Boolean);
  }
  function outputPorts(binding, type) {
    if (!binding) return [];
    const base = {
      binding: binding.id,
      name: binding.name,
      type: type ?? binding.type,
      expression: { variable: binding.name },
      path: [],
    };
    return [
      base,
      ...fieldsOf(base.type).map((field) => ({
        binding: binding.id,
        name: field.name,
        type: field.ty,
        expression: {
          member: { field: { target: base.expression, field: field.name } },
        },
        path: [field.name],
      })),
    ];
  }
  function nodesIn(scope) {
    return view.nodes.filter((node) => siteKey(node.block) === siteKey(scope)).map((carrier) => {
      const owner = rows.get(siteKey(carrier.statement));
      const action = carrier.kind === "action"
        ? data.actions.find((action) => siteKey(action.site) === siteKey(carrier.site))
        : undefined;
      // Selection keeps the exact canvas Site; the statement is supporting
      // inspection/edit context, never a request to pick its last action.
      const s = { ...owner, site: carrier.site, statement_site: owner.site,
        node_kind: carrier.kind, members: carrier.members,
        actions: action ? [action.site] : [] };
      const wholeInputs = [];
      let inputs = [], outputs = [];
      const binding = action && data.bindings.find((binding) =>
        binding.value_site && siteKey(binding.value_site) === siteKey(action.site));
      if (action) {
        inputs = action.args.flatMap((arg, index) => {
          const name = arg.name ?? `argument${index + 1}`;
          const targetBinding = bindings.get(arg.binding);
          const whole = {
            id: `${siteKey(action.site)}:${index}`, name, type: arg.type,
            expression: arg.expression, target: { action: action.site, index },
            context: { statement: owner.site, block: owner.block },
            references: targetBinding ? [targetBinding.id] : [], available: action.available,
          };
          const record = arg.site && expressions.get(siteKey(arg.site));
          if (!record?.expression.record) return [whole];
          if (record.expression.record.length) wholeInputs.push({ ...whole,
            references: record.references, available: record.available });
          return recordPorts(record, arg.type);
        });
        outputs = outputPorts(binding, action.result_type);
        if (!binding) outputs = [{ name: "result", type: action.result_type,
          discarded: !action.destination, stored: !!action.destination, path: [] }];
      } else if (carrier.kind === "code") {
        outputs = [...new Set([...carrier.bindings, ...carrier.writes])].flatMap((id) => outputPorts(bindings.get(id)));
        inputs = carrier.reads.filter((id) => !carrier.bindings.includes(id)).map((id) => ({
          name: bindings.get(id)?.name ?? "value", type: bindings.get(id)?.type ?? "any",
          expression: { variable: bindings.get(id)?.name }, references: [id], available: [],
        }));
      } else if (carrier.header) {
        const expr = expressions.get(siteKey(carrier.header.expression));
        if (expr) inputs = [expressionPort(carrier.header.role, expr.type, expr)];
      }
      const node = { s, carrier, site: carrier.site, action, binding,
        kind: carrier.kind, inputs, outputs, wholeInputs };
      if (carrier.kind === "control" && owner.statement.if) {
        node.branch = true;
        node.children = carrier.arms.map((arm) => nodesIn(arm.block));
      }
      return node;
    });
  }
  const nodes = nodesIn(block);
  const parameters = data.parameters
    .map((id) => bindings.get(id))
    .filter(Boolean);
  return {
    nodes,
    bindings,
    expressions,
    parameters,
    outputPorts,
    rows,
  };
}

export function sourceOptions(view, port) {
  return (port.available ?? []).flatMap((id) => {
    const binding = view.bindings.get(id);
    if (!binding) return [];
    return view.outputPorts(binding).filter((output) => output.expression?.variable !== undefined).map((output) => ({
      ...output,
      label: `${binding.name}${output.path.length ? "." + output.path.join(".") : ""} · ${typeName(output.type)}${binding.assigned ? " · variable" : ""}`,
    }));
  });
}

export function encodeValue(value, type, original) {
  if (original?.literal !== undefined) {
    const previous = constantValue(original);
    if (previous.known && Object.is(previous.value, value)) return original;
  }
  if (value === null) return { literal: "null" };
  if (typeof value === "string") return { literal: { text: value } };
  if (typeof value === "boolean") return { literal: { bool: value } };
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw Error("Use a finite number.");
    if (type === "int" && !Number.isSafeInteger(value))
      throw Error(
        "Use a safe integer, or edit its exact canonical expression.",
      );
    if (
      type !== "float" &&
      Number.isSafeInteger(value) &&
      !Object.is(value, -0)
    )
      return { literal: { int: String(value) } };
    const magnitude = Math.abs(value);
    let text = Object.is(value, -0)
      ? "-0.0"
      : magnitude !== 0 && (magnitude < 1e-4 || magnitude >= 1e16)
        ? value.toExponential().replace("e+", "e")
        : String(value);
    if (!text.includes(".") && !text.includes("e")) text += ".0";
    return { literal: { float: text } };
  }
  if (Array.isArray(value))
    return {
      list: value.map((item, index) =>
        encodeValue(item, type?.list, original?.list?.[index]),
      ),
    };
  return {
    record: Object.entries(value).map(([field, item]) => ({
      field,
      value: encodeValue(
        item,
        fieldsOf(type).find((f) => f.name === field)?.ty,
        original?.record?.find((entry) => entry.field === field)?.value,
      ),
    })),
  };
}

export function availableName(workflow, base = "value") {
  const names = new Set(viewOf(workflow).dataflow.bindings.map((b) => b.name));
  let name = base,
    index = 1;
  while (names.has(name)) name = base + "_" + index++;
  return name;
}
// Build only a closed canvas operation. Source owns arbitrary code changes.
export function planInputEdit(workflow, port, value) {
  if (!port.target) throw Error("This code requires a source edit. Open Edit source, change it and re-import.");
  function data(value) {
    if (value?.literal !== undefined) return !(value.literal && typeof value.literal === "object" && "function" in value.literal);
    if (typeof value?.variable === "string") return true;
    for (const kind of ["list", "tuple", "set"]) if (Array.isArray(value?.[kind])) return value[kind].every(data);
    if (Array.isArray(value?.record)) return value.record.every((entry) => data(entry.value));
    if (Array.isArray(value?.map)) return value.map.every((entry) => data(entry.key) && data(entry.value));
    return false;
  }
  if (!data(value)) throw Error("This computation requires a source edit. Connect a named value or set data here.");
  const base = workflow.identity;
  if (port.target.action) return [{ set_value: { base, target: { tool_argument: { action: port.target.action, index: port.target.index, path: [] } }, value } }];
  const expression = port.target.expression;
  const same = (a, b) => siteKey(a) === siteKey(b);
  const statement = viewOf(workflow).statements.find((s) => same(s.site, port.context.statement));
  if (statement && expression.path.length === statement.site.path.length + 1 && expression.path.at(-1) === 0 && (statement.statement.if || statement.statement.while)) {
    return [{ change_condition: { base, statement: statement.site, condition: value } }];
  }
  for (const action of viewOf(workflow).dataflow.actions) {
    if (action.kind !== "perform") continue;
    for (const [index, arg] of action.args.entries()) {
      if (!arg.site || JSON.stringify(arg.site.unit) !== JSON.stringify(expression.unit) || !arg.site.path.every((p, i) => p === expression.path[i])) continue;
      return [{ set_value: { base, target: { tool_argument: { action: action.site, index, path: expression.path.slice(arg.site.path.length) } }, value } }];
    }
  }
  if (statement?.statement.let && same(expression, { ...statement.site, path: [...statement.site.path, 0] })) {
    return [{ set_value: { base, target: { initializer: { statement: statement.site } }, value } }];
  }
  const slot = viewOf(workflow).dataflow.expressions.find((e) => same(e.site, expression));
  if (slot?.expression.literal !== undefined) return [{ set_value: { base, target: { literal: { expression } }, value } }];
  throw Error("This code requires a source edit. Open Edit source, change it and re-import.");
}
