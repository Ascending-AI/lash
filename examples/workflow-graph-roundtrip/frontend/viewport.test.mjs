import test from "node:test";
import assert from "node:assert/strict";
import {
  MAX_ZOOM,
  MIN_ZOOM,
  fitZoom,
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
  assert.equal(
    fitZoom({ width: 100, height: 100 }, { width: 10000, height: 10 }),
    MIN_ZOOM,
  );
  assert.equal(
    fitZoom({ width: 2000, height: 2000 }, { width: 100, height: 100 }),
    1,
  );
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
