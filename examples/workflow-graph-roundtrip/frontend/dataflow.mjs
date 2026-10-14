import { siteKey, statementsIn } from "./projection.mjs";

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
export function buildDataflow(workflow, block, expanded = new Set()) {
  const data = workflow.dataflow;
  if (!data)
    return {
      nodes: [],
      bindings: new Map(),
      expressions: new Map(),
      hidden: new Set(),
    };
  const bindings = new Map(data.bindings.map((b) => [b.id, b]));
  const expressions = new Map(
    data.expressions.map((e) => [siteKey(e.site), e]),
  );
  const actions = new Map(data.actions.map((a) => [siteKey(a.statement), a]));
  const rows = new Map(workflow.statements.map((s) => [siteKey(s.site), s]));
  const hidden = new Set();
  const folded = new Map();
  for (const action of data.actions) {
    action.args.forEach((arg, index) => {
      const binding = bindings.get(arg.binding);
      const value =
        binding?.valueSite && expressions.get(siteKey(binding.valueSite));
      const siblings = statementsIn(workflow, action.block);
      const previous =
        siblings[
          siblings.findIndex(
            (s) => siteKey(s.site) === siteKey(action.statement),
          ) - 1
        ];
      if (
        binding &&
        !expanded.has(siteKey(binding.site)) &&
        !binding.assigned &&
        !binding.captured &&
        binding.uses.length === 1 &&
        value?.expression?.record &&
        previous &&
        siteKey(previous.site) === siteKey(binding.site)
      ) {
        hidden.add(binding.id);
        folded.set(`${siteKey(action.site)}:${index}`, value);
      }
    });
  }
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
    const nodes = [];
    for (const s of statementsIn(workflow, scope)) {
      const binding = data.bindings.find(
        (b) => b.kind === "let" && siteKey(b.site) === siteKey(s.site),
      );
      if (binding && hidden.has(binding.id)) continue;
      const action = actions.get(siteKey(s.site));
      const returned = data.returns.find(
        (r) => siteKey(r.statement) === siteKey(s.site),
      );
      const assignment = data.assignments?.find(
        (a) => siteKey(a.statement) === siteKey(s.site),
      );
      const wholeInputs = [];
      let inputs = [],
        outputs = [],
        kind = "control";
      if (action) {
        kind = "action";
        inputs = action.args.flatMap((arg, index) => {
          const record = folded.get(`${siteKey(action.site)}:${index}`);
          if (record) {
            if (record.expression.record.length)
              wholeInputs.push(expressionPort(arg.name, arg.type, record));
            return recordPorts(record, arg.type);
          }
          const targetBinding = bindings.get(arg.binding);
          // Arguments are atoms. The helper materializes a computed replacement.
          return [
            {
              id: `${siteKey(action.site)}:${index}`,
              name: arg.name,
              type: arg.type,
              expression: arg.atom,
              target: { action: action.site, index },
              context: { statement: s.site, block: s.block },
              references: targetBinding ? [targetBinding.id] : [],
              available: action.available ?? [],
            },
          ];
        });
        outputs = outputPorts(binding, action.resultType);
        if (!binding)
          outputs = [
            {
              name: action.destination
                ? `writes ${expressionName(action.destination)}`
                : "result",
              type: action.resultType,
              discarded: !action.destination,
              stored: !!action.destination,
              path: [],
            },
          ];
      } else if (returned) {
        kind = "output";
        const expr = expressions.get(siteKey(returned.expressionSite));
        if (expr)
          inputs = [
            {
              ...expressionPort("result", returned.type, expr),
              acceptsType: returned.expectedType ?? "any",
            },
          ];
      } else if (binding) {
        kind = "value";
        const expr =
          binding.valueSite && expressions.get(siteKey(binding.valueSite));
        if (expr)
          inputs = expr.expression.record
            ? recordPorts(expr, binding.type)
            : [expressionPort("value", binding.type, expr)];
        inputs = inputs.map((port) => ({ ...port, acceptsType: "any" }));
        if (expr?.expression.record?.length)
          wholeInputs.push({
            ...expressionPort("value", binding.type, expr),
            acceptsType: "any",
          });
        outputs = outputPorts(binding);
      } else if (assignment) {
        kind = "write";
        const expr = expressions.get(siteKey(assignment.valueSite));
        if (expr)
          inputs = [
            { ...expressionPort("value", expr.type, expr), acceptsType: "any" },
          ];
        outputs = [
          {
            name: `writes ${expressionName(assignment.destination)}`,
            type: expr?.type ?? "any",
            stored: true,
            path: [],
          },
        ];
      } else if (s.statement.if || s.statement.while) {
        const expr = data.expressions.find(
          (e) =>
            siteKey(e.statement) === siteKey(s.site) &&
            siteKey(e.site) ===
              siteKey({ ...s.site, path: [...s.site.path, 0] }),
        );
        if (expr) inputs = [expressionPort("condition", "bool", expr)];
      } else {
        const owned = data.expressions.filter(
          (e) => siteKey(e.statement) === siteKey(s.site),
        );
        const roots = owned.filter(
          (expr) =>
            !owned.some(
              (parent) =>
                parent.site.path.length < expr.site.path.length &&
                parent.site.path.every(
                  (part, index) => part === expr.site.path[index],
                ),
            ),
        );
        const tag =
          typeof s.statement === "string"
            ? s.statement
            : Object.keys(s.statement)[0];
        inputs = roots.map((expr, index) =>
          expressionPort(
            tag === "for"
              ? "iterable"
              : roots.length === 1
                ? "value"
                : `input ${index + 1}`,
            expr.type,
            expr,
          ),
        );
      }
      const node = {
        s,
        binding,
        kind,
        inputs,
        outputs,
        wholeInputs,
        terminal: returned
          ? s.statement.finish
            ? "finish"
            : "return"
          : undefined,
        workflowOutput: returned ? !returned.callable?.path?.length : false,
      };
      if (s.statement.if) {
        node.branch = true;
        node.children = [1, 2].map((i) =>
          nodesIn({ ...s.site, path: [...s.site.path, i] }),
        );
      }
      nodes.push(node);
      const tag =
        typeof s.statement === "string"
          ? s.statement
          : Object.keys(s.statement)[0];
      if (
        ["return", "throw", "finish", "fail", "break", "continue"].includes(tag)
      )
        break;
    }
    return nodes;
  }
  const nodes = nodesIn(block);
  const parameters = data.parameters
    .map((id) => bindings.get(id))
    .filter(Boolean);
  return {
    nodes,
    bindings,
    expressions,
    hidden,
    parameters,
    outputPorts,
    rows,
  };
}

export function sourceOptions(view, port) {
  return (port.available ?? []).flatMap((id) => {
    const binding = view.bindings.get(id);
    if (!binding || view.hidden.has(id)) return [];
    return view.outputPorts(binding).map((output) => ({
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
  const names = new Set(workflow.dataflow.bindings.map((b) => b.name));
  let name = base,
    index = 1;
  while (names.has(name)) name = base + "_" + index++;
  return name;
}
function inputSteps(workflow, port, value) {
  if (port.target.expression)
    return [
      {
        direct: {
          replace_expression: {
            expression: port.target.expression,
            with: value,
          },
        },
      },
    ];
  if (value.variable || value.literal !== undefined)
    return [
      {
        direct: {
          set_argument: {
            action: port.target.action,
            index: port.target.index,
            argument: value,
          },
        },
      },
    ];
  const name = availableName(workflow, "input_value");
  return [
    {
      direct: {
        insert_statement: {
          at: { block: port.context.block, before: port.context.statement },
          statement: { let: { name, value: { expr: value } } },
        },
      },
    },
    {
      direct: {
        set_argument: {
          action: port.target.action,
          index: port.target.index,
          argument: { variable: name },
        },
      },
    },
  ];
}

export function planInputEdit(workflow, port, value) {
  return inputSteps(workflow, port, value).map((step) => step.direct);
}

export function planConstantPromotion(workflow, port, value, name) {
  if (!constantValue(value).known)
    throw Error(
      "Only constants can become named values. Keep connected values and expressions at their original evaluation site.",
    );
  if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(name))
    throw Error("Use a name starting with a letter or underscore.");
  if (workflow.dataflow.bindings.some((binding) => binding.name === name))
    throw Error("Choose a new value name.");
  return [
    {
      insert_statement: {
        at: { block: port.context.block, before: port.context.statement },
        statement: { let: { name, value: { expr: value } } },
      },
    },
    ...planInputEdit(workflow, port, { variable: name }),
  ];
}

// The source remains a normal step. Termination is a separate canonical statement.
export function planEndAfter(workflow, node, source, operation) {
  if (!["return", "finish"].includes(operation))
    throw Error("Choose Return or Finish.");
  if (
    !node.s ||
    !node.outputs.some(
      (port) =>
        port.binding === source.binding &&
        !port.discarded &&
        !port.stored &&
        JSON.stringify(port.expression) === JSON.stringify(source.expression),
    )
  )
    throw Error("Choose a named output from this step.");
  const siblings = statementsIn(workflow, node.s.block);
  const index = siblings.findIndex(
    (s) => siteKey(s.site) === siteKey(node.s.site),
  );
  if (index < 0) throw Error("The source step is no longer in this document.");
  const next = siblings[index + 1];
  const statement = { [operation]: { value: source.expression } };
  if (next?.statement.return || next?.statement.finish)
    return [{ replace_statement: { statement: next.site, with: statement } }];
  return [
    {
      insert_statement: {
        at: { block: node.s.block, ...(next ? { before: next.site } : {}) },
        statement,
      },
    },
  ];
}
