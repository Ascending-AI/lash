#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const PINNED_COMMIT = "3655e7464de3d52643ecddd4b5f9f4f3e7f62398";
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
const typescriptNames = [
  "annotations",
  "as-casts",
  "decorators",
  "enum",
  "generics",
  "interfaces",
  "namespaces",
  "non-null-assertion",
  "satisfies",
  "type-aliases",
];
const inventory = [
  ...directoryNames.map((name) => ["directory", name]),
  ...featureNames.map((name) => ["feature", name]),
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

const censusRows = rows(join(root, "census.tsv"), 4);
const census = new Map(censusRows.map(([kind, name, status, reason]) => [`${kind}:${name}`, { status, reason }]));
const inventoryKeys = new Set(inventory.trimEnd().split("\n").map((line) => line.replace("\t", ":")));
if (census.size !== censusRows.length) throw new Error("census.tsv has duplicate entries");
if (census.size !== inventoryKeys.size || [...inventoryKeys].some((key) => !census.has(key))) {
  throw new Error("census.tsv does not exactly cover inventory.tsv; classify every row before syncing");
}

const manifest = rows(join(root, "manifest.tsv"), 4).map(([path, area, disposition, expectation]) => ({
  path,
  area,
  disposition,
  expectation,
}));
const manifestByPath = new Map(manifest.map((entry) => [entry.path, entry]));
if (manifestByPath.size !== manifest.length) throw new Error("manifest.tsv has duplicate paths");

const allTests = walk(join(source, "test"))
  .filter((path) => path.endsWith(".js") && !path.endsWith("FIXTURE.js"))
  .map((path) => relative(source, path).replaceAll("\\", "/"))
  .sort();
const allTestSet = new Set(allTests);
for (const entry of manifest) {
  if (!allTestSet.has(entry.path)) throw new Error(`manifest path does not exist upstream: ${entry.path}`);
}

const supportedHarness = new Set(["assert.js", "sta.js", "compareArray.js", "propertyHelper.js"]);
function inferredReason(testPath, code, meta) {
  const manifestEntry = manifestByPath.get(testPath);
  if (manifestEntry?.disposition === "skip") return `expected-rejection:${manifestEntry.expectation}`;
  const top = testPath.split("/")[1];
  const directory = census.get(`directory:${top}`);
  if (directory.status !== "accepted") return `${directory.status}:${directory.reason}`;
  for (const feature of meta.features) {
    const entry = census.get(`feature:${feature}`);
    if (!entry) throw new Error(`${testPath} uses uncensused feature ${feature}`);
    if (entry.status !== "accepted") return `${entry.status}:${entry.reason}`;
  }
  const unsupportedInclude = meta.includes.find((include) => !supportedHarness.has(include));
  if (unsupportedInclude) return `harness-uses:${unsupportedInclude}`;
  const patterns = [
    [/\bvar\b/, "out-of-dialect:TS_VAR_UNSUPPORTED"],
    [/\bclass\b/, "out-of-dialect:TS_CLASS_UNSUPPORTED"],
    [/\b(?:function\s*\*|yield\b)/, "out-of-dialect:TS_GENERATOR_UNSUPPORTED"],
    [/\basync\b/, "out-of-dialect:TS_ASYNC_UNSUPPORTED"],
    [/\?\./, "out-of-dialect:TS_OPTIONAL_CHAINING_UNSUPPORTED"],
    [/\.\.\./, "out-of-dialect:TS_SPREAD_UNSUPPORTED"],
    [/\bnew\s+/, "out-of-dialect:TS_NEW_UNSUPPORTED"],
    [/\bswitch\s*\(/, "out-of-dialect:TS_SWITCH_UNSUPPORTED"],
    [/(?:\+\+|--)/, "out-of-dialect:TS_UPDATE_UNSUPPORTED"],
    [/(?:\+=|-=|\*=|\/=|%=|&&=|\|\|=|\?\?=)/, "out-of-dialect:TS_ASSIGNMENT_OPERATOR_UNSUPPORTED"],
    [/\bfor\s*\([^;]*\bin\b/, "out-of-dialect:TS_FOR_IN_UNSUPPORTED"],
    [/\bdebugger\b/, "out-of-dialect:TS_DEBUGGER_UNSUPPORTED"],
  ];
  for (const [pattern, reason] of patterns) if (pattern.test(code)) return reason;
  return "skip:ticket-ruling:FIG-1413-initial-subset";
}

const skips = [];
for (const testPath of allTests) {
  const manifestEntry = manifestByPath.get(testPath);
  if (manifestEntry?.disposition === "pass") continue;
  const code = readFileSync(join(source, testPath), "utf8");
  skips.push([testPath, inferredReason(testPath, code, frontmatter(code))]);
}
for (const entry of manifest.filter((candidate) => candidate.disposition === "pass")) {
  const meta = frontmatter(readFileSync(join(source, entry.path), "utf8"));
  if (!meta.flags.some((flag) => new Set(["noStrict", "onlyStrict", "raw"]).has(flag))) {
    skips.push([`${entry.path}#strict`, "strict-mode-variant:n.a."]);
  }
}
skips.sort(([pathA], [pathB]) => pathA.localeCompare(pathB));
const skipRegister = "# test262-path\treason\n" + skips.map((row) => row.join("\t")).join("\n") + "\n";
compareOrWrite(join(root, "skip-register.tsv"), skipRegister);
compareOrWrite(join(root, "upstream-test-count.txt"), `${allTests.length}\n`);

if (mode === "sync") {
  const vendoredRoot = join(root, "test");
  const wanted = new Set(manifest.map((entry) => entry.path));
  if (existsSync(vendoredRoot)) {
    for (const path of walk(vendoredRoot).filter((path) => path.endsWith(".js"))) {
      const relativePath = `test/${relative(vendoredRoot, path).replaceAll("\\", "/")}`;
      if (!wanted.has(relativePath)) unlinkSync(path);
    }
  }
  for (const entry of manifest) {
    const destination = join(root, entry.path);
    mkdirSync(dirname(destination), { recursive: true });
    copyFileSync(join(source, entry.path), destination);
  }
  mkdirSync(join(root, "harness"), { recursive: true });
  for (const harness of supportedHarness) {
    copyFileSync(join(source, "harness", harness), join(root, "harness", harness));
  }
  copyFileSync(join(source, "LICENSE"), join(root, "LICENSE"));
} else {
  for (const entry of manifest) {
    const vendored = join(root, entry.path);
    if (!existsSync(vendored) || readFileSync(vendored, "utf8") !== readFileSync(join(source, entry.path), "utf8")) {
      throw new Error(`${entry.path} does not match pinned upstream`);
    }
  }
  for (const harness of supportedHarness) {
    if (readFileSync(join(root, "harness", harness), "utf8") !== readFileSync(join(source, "harness", harness), "utf8")) {
      throw new Error(`harness/${harness} does not match pinned upstream`);
    }
  }
}

const strictVariantSkips = skips.filter(([, reason]) => reason === "strict-mode-variant:n.a.").length;
console.log(
  `test262 ${mode}: ${allTests.length} upstream, ${manifest.length} vendored, ` +
    `${skips.length - strictVariantSkips} path skips, ${strictVariantSkips} strict variants`,
);
