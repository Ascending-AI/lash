// Records, from Node, the order in which each case of cases.tsv evaluates
// its marked operands, and writes order.tsv beside it.
//
// A case is a script that reads two bindings. `m.<name>(value)` records
// <name> and gives `value` back, so a marked operand has a side effect the
// record sees and the value the surrounding expression needs. `o` is an
// object with a number, an identity function and nothing else.
//
// Run from this directory with Node: `node record.mjs`.
import { readFileSync, writeFileSync } from "node:fs";

const here = new URL(".", import.meta.url);
const lines = readFileSync(new URL("cases.tsv", here), "utf8")
  .split("\n")
  .filter((line) => line !== "" && !line.startsWith("#"));

let out = "# family\torder Node evaluates the marked operands in\n";
for (const line of lines) {
  const [family, source] = line.split("\t");
  const order = [];
  const m = new Proxy(
    {},
    {
      get: (_, name) => (value) => {
        order.push(name);
        return value;
      },
    },
  );
  const o = { n: 1, id: (value) => value };
  new Function("m", "o", `"use strict";\n${source}`)(m, o);
  out += `${family}\t${order.join(" ")}\n`;
}
writeFileSync(new URL("order.tsv", here), out);
