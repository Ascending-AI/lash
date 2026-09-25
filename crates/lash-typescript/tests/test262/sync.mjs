#!/usr/bin/env node

// Derives the executable Test262 selection from the pinned upstream checkout
// and the census, and vendors it. Nothing here is hand-picked: a test is
// selected exactly when every census row it touches is `accepted` (see
// README.md, "Selection"). `inventory` refreshes only inventory.tsv; `sync`
// rewrites every derived file and the vendored tree; `check` rewrites nothing
// and fails when any derived file or vendored byte differs from what `sync`
// would write.

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmdirSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const PINNED_COMMIT = "3655e7464de3d52643ecddd4b5f9f4f3e7f62398";
// The PR lane's sample size. Each second-level directory contributes
// round(count * SAMPLE_TARGET / selected), at least one test, chosen in
// SHA-256 order of the path, so the sample is stable across runs and moves
// only when the selection does.
const SAMPLE_TARGET = 500;
const root = dirname(fileURLToPath(import.meta.url));
const [mode, sourceArgument] = process.argv.slice(2);
if (!new Set(["inventory", "sync", "check"]).has(mode) || !sourceArgument) {
  throw new Error("usage: node tests/test262/sync.mjs <inventory|sync|check> <test262-checkout>");
}
const source = resolve(sourceArgument);
const actualCommit = execFileSync("git", ["-C", source, "rev-parse", "HEAD"], {
  encoding: "utf8",
}).trim();
if (actualCommit !== PINNED_COMMIT) {
  throw new Error(`expected test262 ${PINNED_COMMIT}, got ${actualCommit}`);
}

function walk(directory) {
  const files = [];
  for (const name of readdirSync(directory).sort()) {
    const path = join(directory, name);
    if (statSync(path).isDirectory()) files.push(...walk(path));
    else files.push(path);
  }
  return files;
}

function rows(path, columns) {
  return readFileSync(path, "utf8")
    .split(/\r?\n/)
    .filter((line) => line && !line.startsWith("#"))
    .map((line) => {
      const fields = line.split("\t");
      if (fields.length !== columns) throw new Error(`${path}: malformed row: ${line}`);
      return fields;
    });
}

function frontmatter(code) {
  const match = code.match(/\/\*---([\s\S]*?)---\*\//);
  if (!match) throw new Error("test has no Test262 frontmatter");
  const yaml = match[1].replace(/\r/g, "\n");
  const array = (key) => {
    const inline = yaml.match(new RegExp(`^${key}:\\s*\\[([^\\]]*)\\]`, "m"));
    if (inline) {
      return inline[1]
        .split(",")
        .map((value) => value.trim().replace(/^['"]|['"]$/g, ""))
        .filter(Boolean);
    }
    const block = yaml.match(new RegExp(`^${key}:\\s*\\n((?:\\s+-[^\\n]*\\n?)*)`, "m"));
    return block
      ? block[1]
          .split(/\n/)
          .map((line) => line.replace(/^\s+-\s*/, "").trim())
          .filter(Boolean)
      : [];
  };
  return { features: array("features"), includes: array("includes"), flags: array("flags") };
}

const featureNames = readFileSync(join(source, "features.txt"), "utf8")
  .split(/\r?\n/)
  .map((line) => line.split("#", 1)[0].trim())
  .filter(Boolean);
const directoryNames = readdirSync(join(source, "test"))
  .filter((name) => statSync(join(source, "test", name)).isDirectory())
  .sort();
// Every flag INTERPRETING.md defines. A test carrying any other flag stops the
// sync: an unknown flag has no ruling.
const flagNames = [
  "CanBlockIsFalse",
  "CanBlockIsTrue",
  "async",
  "generated",
  "module",
  "noStrict",
  "non-deterministic",
  "onlyStrict",
  "raw",
];
// Dialect decisions no upstream feature tag carries. Most are TypeScript-only
// syntax; the rest are ECMAScript constructs whose refusal no tag names (ES5
// syntax carries no feature tag at all), so the census needs a row of its own
// for each refusal the selection shows.
const typescriptNames = [
  // Object-literal `get`/`set` (FIG-3646): ES5 syntax, so no feature tag.
  "accessors",
  "annotations",
  "as-casts",
  // Not TypeScript syntax: a dialect decision with no upstream feature tag.
  // The async array driver runs callbacks sequentially, which the census
  // records as `registered-deviation:TS_ASYNC_MAP_SEQUENTIAL_V1` because there
  // is no other row that can carry it (FIG-3392).
  "async-array-callbacks",
  "decorators",
  "enum",
  "generics",
  "interfaces",
  "namespaces",
  "non-null-assertion",
  // Not a feature tag upstream: `Promise.race` rides the base `Promise`
  // feature, which the census skips as a whole. The dialect's ruling on it
  // needs a row of its own (FIG-3397, ADR 0099 §11).
  "Promise.race",
  "satisfies",
  "type-aliases",
  // ECMAScript constructs and operations the dialect refuses with no
  // upstream feature tag to hang the ruling on (FIG-3646). Each names the
  // refusal the selection shows, so every `refused` outcome maps to a row.
  "array-holes",
  "array-index-delete",
  "array-named-properties",
  "async-calls-unawaited",
  "binding-reassignment",
  "builtin-arity",
  // Members a built-in namespace lacks: tsc --strict rejects the same reads
  // and writes as TS2339 (FIG-3705).
  "builtin-member-read",
  "builtin-member-write",
  "classic-for-forms",
  "closed-shape-field-guard",
  "comma-operator",
  // Arity past a constructor's signature: `new Map(1, 2)` refuses at run
  // time, the same shape tsc --strict rejects as TS2554 (FIG-3705).
  "constructor-arity",
  "date-mutation",
  "date-string-coercion",
  "debugger",
  // Forms `tsc --strict` rejects, refused rather than implemented (FIG-3651,
  // ADR 0064).
  "delete-non-reference",
  // Destructuring or spreading a non-iterable (`const [a] = null;`) faults
  // at run time; tsc --strict rejects it as TS2488 (FIG-3705).
  "destructuring-non-iterable",
  "direct-eval",
  "function-constructor",
  "function-redeclaration",
  "instanceof-arbitrary",
  "labels",
  "lone-surrogate-values",
  "matchall-iterator-position",
  // Arity short of a listed method's signature: `[1].map()` refuses, the
  // same shape tsc --strict rejects as TS2554 (FIG-3705).
  "method-arity",
  "mutable-captures",
  "new-arbitrary",
  "object-string-coercion",
  "private-names-outside-classes",
  // A `new RegExp` pattern that is not a string literal: tsc --strict
  // rejects `new RegExp(null)` as TS2769 (FIG-3705).
  "regexp-pattern-argument",
  "reserved-identifiers",
  "source-nesting",
  "tagged-templates",
  "temporal-dead-zone",
  "this",
  "unresolvable-references",
  "with",
  "source-size",
];
const inventory =
  [
    ...directoryNames.map((name) => ["directory", name]),
    ...featureNames.map((name) => ["feature", name]),
    ...flagNames.map((name) => ["flag", name]),
    ...typescriptNames.map((name) => ["typescript", name]),
  ]
    .sort(([kindA, nameA], [kindB, nameB]) => kindA.localeCompare(kindB) || nameA.localeCompare(nameB))
    .map((fields) => fields.join("\t"))
    .join("\n") + "\n";

function compareOrWrite(path, contents) {
  if (mode === "check") {
    if (!existsSync(path) || readFileSync(path, "utf8") !== contents) {
      throw new Error(`${path} is stale; run sync mode`);
    }
  } else {
    writeFileSync(path, contents);
  }
}

compareOrWrite(join(root, "inventory.tsv"), inventory);
if (mode === "inventory") process.exit(0);

const censusRows = rows(join(root, "census.tsv"), 5);
const census = new Map(censusRows.map(([kind, name, status, reason]) => [`${kind}:${name}`, { status, reason }]));
const inventoryKeys = new Set(inventory.trimEnd().split("\n").map((line) => line.replace("\t", ":")));
if (census.size !== censusRows.length) throw new Error("census.tsv has duplicate entries");
if (census.size !== inventoryKeys.size || [...inventoryKeys].some((key) => !census.has(key))) {
  throw new Error("census.tsv does not exactly cover inventory.tsv; classify every row before syncing");
}

const allTests = walk(join(source, "test"))
  .filter((path) => path.endsWith(".js") && !path.endsWith("FIXTURE.js"))
  .map((path) => relative(source, path).replaceAll("\\", "/"))
  .sort();

// The census row that keeps a test out of the selection, or null when every
// row it touches is accepted. Rules apply in this order, and the first
// non-accepted row names the exclusion:
// 1. its top-level directory;
// 2. for `test/built-ins/<X>/...`, the feature row named `<X>` when there is
//    one: upstream tags are incomplete, and an untagged test under
//    `built-ins/Promise` exercises Promise all the same;
// 3. each of its flags, in file order;
// 4. each of its features, in file order.
function exclusion(testPath, meta) {
  const [, top, builtIn] = testPath.split("/");
  const rules = [`directory:${top}`];
  if (top === "built-ins" && census.has(`feature:${builtIn}`)) rules.push(`feature:${builtIn}`);
  for (const flag of meta.flags) {
    if (!census.has(`flag:${flag}`)) throw new Error(`${testPath} carries uncensused flag ${flag}`);
    rules.push(`flag:${flag}`);
  }
  for (const feature of meta.features) {
    if (!census.has(`feature:${feature}`)) throw new Error(`${testPath} uses uncensused feature ${feature}`);
    rules.push(`feature:${feature}`);
  }
  return rules.find((rule) => census.get(rule).status !== "accepted") ?? null;
}

const selected = [];
const skips = [];
const includes = new Set(["assert.js", "sta.js", "doneprintHandle.js"]);
for (const testPath of allTests) {
  const top = testPath.split("/")[1];
  if (census.get(`directory:${top}`).status !== "accepted") {
    skips.push([testPath, `directory:${top}`]);
    continue;
  }
  const meta = frontmatter(readFileSync(join(source, testPath), "utf8"));
  const rule = exclusion(testPath, meta);
  if (rule) {
    skips.push([testPath, rule]);
  } else {
    selected.push(testPath);
    for (const include of meta.includes) includes.add(include);
  }
}

const strata = new Map();
for (const testPath of selected) {
  const stratum = testPath.split("/").slice(1, 3).join("/");
  if (!strata.has(stratum)) strata.set(stratum, []);
  strata.get(stratum).push(testPath);
}
const sample = [];
for (const [, paths] of [...strata].sort(([a], [b]) => a.localeCompare(b))) {
  const take = Math.max(1, Math.round((paths.length * SAMPLE_TARGET) / selected.length));
  sample.push(
    ...paths
      .map((path) => [createHash("sha256").update(path).digest("hex"), path])
      .sort(([a], [b]) => a.localeCompare(b))
      .slice(0, take)
      .map(([, path]) => path),
  );
}
sample.sort();

compareOrWrite(
  join(root, "skip-register.tsv"),
  "# test262-path\texcluding-census-row\n" + skips.map((row) => row.join("\t")).join("\n") + "\n",
);
compareOrWrite(join(root, "sample.tsv"), "# test262-path\n" + sample.join("\n") + "\n");
compareOrWrite(join(root, "upstream-test-count.txt"), `${allTests.length}\n`);

const vendoredRoot = join(root, "test");
const vendored = existsSync(vendoredRoot)
  ? walk(vendoredRoot).map((path) => `test/${relative(vendoredRoot, path).replaceAll("\\", "/")}`)
  : [];
const wanted = new Set(selected);
const harnessRoot = join(root, "harness");
const vendoredHarness = existsSync(harnessRoot) ? readdirSync(harnessRoot).sort() : [];
if (mode === "sync") {
  for (const path of vendored.filter((path) => !wanted.has(path))) unlinkSync(join(root, path));
  for (const directory of walkDirectories(vendoredRoot).reverse()) {
    if (readdirSync(directory).length === 0) rmdirSync(directory);
  }
  for (const testPath of selected) {
    const destination = join(root, testPath);
    mkdirSync(dirname(destination), { recursive: true });
    copyFileSync(join(source, testPath), destination);
  }
  mkdirSync(harnessRoot, { recursive: true });
  for (const name of vendoredHarness.filter((name) => !includes.has(name))) unlinkSync(join(harnessRoot, name));
  for (const harness of includes) copyFileSync(join(source, "harness", harness), join(harnessRoot, harness));
  copyFileSync(join(source, "LICENSE"), join(root, "LICENSE"));
} else {
  const extra = vendored.filter((path) => !wanted.has(path));
  if (extra.length) throw new Error(`vendored tests outside the selection: ${extra.slice(0, 5).join(", ")}`);
  for (const testPath of selected) {
    const copy = join(root, testPath);
    if (!existsSync(copy) || !readFileSync(copy).equals(readFileSync(join(source, testPath)))) {
      throw new Error(`${testPath} does not match pinned upstream`);
    }
  }
  const expectedHarness = [...includes].sort();
  if (vendoredHarness.join("\n") !== expectedHarness.join("\n")) {
    throw new Error(`harness/ must hold exactly ${expectedHarness.join(", ")}`);
  }
  for (const harness of includes) {
    if (!readFileSync(join(harnessRoot, harness)).equals(readFileSync(join(source, "harness", harness)))) {
      throw new Error(`harness/${harness} does not match pinned upstream`);
    }
  }
  if (!readFileSync(join(root, "LICENSE")).equals(readFileSync(join(source, "LICENSE")))) {
    throw new Error("LICENSE does not match pinned upstream");
  }
}

function walkDirectories(directory) {
  if (!existsSync(directory)) return [];
  const directories = [directory];
  for (const name of readdirSync(directory).sort()) {
    const path = join(directory, name);
    if (statSync(path).isDirectory()) directories.push(...walkDirectories(path));
  }
  return directories;
}

console.log(
  `test262 ${mode}: ${allTests.length} upstream, ${selected.length} selected, ` +
    `${sample.length} sampled, ${skips.length} excluded by census rows`,
);
