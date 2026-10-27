import test from "node:test";
import assert from "node:assert/strict";
import {
  armCaption,
  captionFrom,
  captionSite,
  conditionText,
  nodeCaptions,
  withCaption,
  workflowTitle,
} from "./captions.mjs";

const site = (path) => ({ unit: { function: "demo" }, path });
const caption = (title) => ({ title });

// EXAMPLE-CAPTIONS: the canvas shows lash's join by view location, marks the
// captions an edit touched, and titles the workflow by its entry's caption.
test("EXAMPLE-CAPTIONS reads captions where lash's caption view puts them", () => {
  const workflow = {
    entry: "demo",
    captionsToReview: [site([1])],
    captionView: {
      nodes: [
        { node: 0, site: site([0]), caption: caption("Send the report") },
        { node: 2, site: site([2]), caption: caption("Tidy up") },
        { node: 2, site: site([3]), caption: caption("Say goodbye") },
        { node: 1, site: site([1]), caption: caption("Check the flag") },
      ],
      arms: [{ node: 1, arm: 1, block: site([1, 2]), caption: caption("Otherwise") }],
      callables: [{ callable: 0, block: site([]), caption: caption("Weekly report") }],
    },
  };
  assert.deepEqual(nodeCaptions(workflow, 0), [
    { site: site([0]), caption: caption("Send the report"), review: false },
  ]);
  assert.deepEqual(nodeCaptions(workflow, 1), [
    { site: site([1]), caption: caption("Check the flag"), review: true },
  ]);
  // A code node shows every captioned member, in lash's order.
  assert.deepEqual(nodeCaptions(workflow, 2).map((item) => item.caption.title), ["Tidy up", "Say goodbye"]);
  assert.deepEqual(nodeCaptions(workflow, 3), []);
  assert.deepEqual(armCaption(workflow, 1, 1), caption("Otherwise"));
  assert.equal(armCaption(workflow, 1, 0), undefined);
  assert.deepEqual(workflowTitle(workflow), caption("Weekly report"));
  assert.equal(workflowTitle({ ...workflow, entry: "other" }), undefined);
  // A version with no view has no join: every step uses the host's wording.
  assert.deepEqual(nodeCaptions({ entry: "demo" }, 0), []);
});

// EXAMPLE-CAPTIONS: a rename builds the whole new entry list and changes
// nothing else; a step is captioned on its statement, and only the node of a
// statement's first action may carry it.
test("EXAMPLE-CAPTIONS renames, adds and removes one caption by building a new list", () => {
  const captions = {
    document: "d",
    entries: [
      { site: site([0]), caption: caption("Send the report") },
      { site: site([1]), caption: caption("Check the flag") },
    ],
  };
  const before = structuredClone(captions);
  assert.deepEqual(withCaption(captions, site([0]), caption("Email the team")), [
    { site: site([1]), caption: caption("Check the flag") },
    { site: site([0]), caption: caption("Email the team") },
  ]);
  assert.deepEqual(withCaption(captions, site([1]), undefined), [
    { site: site([0]), caption: caption("Send the report") },
  ]);
  assert.equal(withCaption(captions, site([4]), caption("New")).length, 3);
  assert.deepEqual(captions, before);
  assert.deepEqual(captionFrom("  Email the team ", " \n"), { title: "Email the team" });
  assert.deepEqual(captionFrom("Email", " Only leads. "), { title: "Email", description: "Only leads." });
  const statement = site([2]);
  assert.deepEqual(captionSite({ body: { kind: "action", members: [statement, site([2, 0])] }, statement }), statement);
  assert.equal(captionSite({ body: { kind: "action", members: [] }, statement }), undefined);
  assert.deepEqual(captionSite({ body: { kind: "code", members: [site([3]), site([4])] }, statement }), site([3]));
});

// EXAMPLE-CAPTIONS: an uncaptioned condition is worded by the condition its
// printed line tests, in either dialect.
test("EXAMPLE-CAPTIONS words an uncaptioned condition by its printed test", () => {
  assert.equal(conditionText("  if (value < 0) {"), "value < 0");
  assert.equal(conditionText("    if value < 0:"), "value < 0");
  assert.equal(conditionText("if (a) && (b) {"), "(a) && (b)");
  assert.equal(conditionText(undefined), undefined);
});
