import test from "node:test";
import assert from "node:assert/strict";
import {
  MAX_ZOOM,
  MIN_ZOOM,
  dragged,
  frame,
  neighbour,
  pinchStep,
  wheelIntent,
  zoomAt,
} from "./viewport.mjs";

test("zooming around the cursor keeps the scene point under it in place", () => {
  const before = { zoom: 0.5, scrollLeft: 120, scrollTop: 40 };
  const cursor = { x: 300, y: 200 };
  const after = zoomAt(before, cursor, 1.25);
  const scene = (view) => [
    (view.scrollLeft + cursor.x) / view.zoom,
    (view.scrollTop + cursor.y) / view.zoom,
  ];
  assert.equal(after.zoom, 1.25);
  assert.deepEqual(scene(after), scene(before));
});

test("wheel zoom stays within the canvas zoom bounds", () => {
  const view = { zoom: 1, scrollLeft: 0, scrollTop: 0 };
  assert.equal(zoomAt(view, { x: 0, y: 0 }, 9).zoom, MAX_ZOOM);
  assert.equal(zoomAt(view, { x: 0, y: 0 }, 0.01).zoom, MIN_ZOOM);
  const fit = (viewport, graph) =>
    frame(viewport, { x: 0, y: 0, ...graph }, 30).zoom;
  assert.equal(fit({ width: 100, height: 100 }, { width: 10000, height: 10 }), MIN_ZOOM);
  assert.equal(fit({ width: 2000, height: 2000 }, { width: 100, height: 100 }), 1);
});

test("pinch and mouse wheels zoom while two-finger trackpad scrolls pan", () => {
  const wheel = (fields) => ({
    ctrlKey: false,
    metaKey: false,
    deltaMode: 0,
    deltaX: 0,
    deltaY: 0,
    ...fields,
  });
  assert.equal(wheelIntent(wheel({ ctrlKey: true, deltaY: 2.5 })), "zoom");
  assert.equal(wheelIntent(wheel({ deltaMode: 1, deltaY: 3 })), "zoom");
  assert.equal(wheelIntent(wheel({ deltaY: 100 })), "zoom");
  assert.equal(wheelIntent(wheel({ deltaY: 12.5 })), "pan");
  assert.equal(wheelIntent(wheel({ deltaY: 4 })), "pan");
  assert.equal(wheelIntent(wheel({ deltaX: 3, deltaY: 100 })), "pan");
});

test("a press is a click until it moves past the slop, then a drag", () => {
  const start = { x: 100, y: 100 };
  assert.equal(dragged(start, { x: 102, y: 102 }), false);
  assert.equal(dragged(start, { x: 103, y: 100 }), false);
  assert.equal(dragged(start, { x: 103, y: 101 }), true);
  assert.equal(dragged(start, { x: 96, y: 100 }), true);
});

test("a two-finger pinch zooms by the spread around the midpoint and pans with it", () => {
  const step = pinchStep(
    [{ x: 100, y: 100 }, { x: 200, y: 100 }],
    [{ x: 60, y: 120 }, { x: 260, y: 120 }],
  );
  assert.deepEqual(step, { factor: 2, center: { x: 160, y: 120 }, pan: { x: 10, y: 20 } });
});

test("arrow keys move to the nearest node in their direction, not one off to the side", () => {
  const from = { x: 0, y: 0 };
  const right = { x: 300, y: 0 },
    farRight = { x: 600, y: 0 },
    diagonal = { x: 200, y: 180 },
    below = { x: 20, y: 300 };
  const nodes = [from, right, farRight, diagonal, below];
  assert.equal(neighbour(nodes, from, "ArrowRight"), right);
  assert.equal(neighbour(nodes, from, "ArrowDown"), below);
  assert.equal(neighbour(nodes, from, "ArrowLeft"), undefined);
  assert.equal(neighbour(nodes, right, "ArrowLeft"), from);
});

test("zooming to a selection centres it, never past 100%", () => {
  const view = frame({ width: 1000, height: 600 }, { x: 400, y: 300, width: 200, height: 100 });
  assert.deepEqual(view, { zoom: 1, scrollLeft: 0, scrollTop: 50 });
});
