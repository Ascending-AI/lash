// Canvas zoom: what a wheel gesture means, and how zooming keeps the point
// under the cursor in place.

export const MIN_ZOOM = 0.3;
export const MAX_ZOOM = 1.6;

export function clampZoom(zoom) {
  return Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, zoom));
}

// A pinch arrives as ctrl+wheel. A mouse wheel scrolls by lines or pages,
// or by whole notches of 50px or more with no horizontal part; a trackpad's
// two-finger scroll sends small or fractional pixel deltas and pans.
export function wheelIntent(event) {
  if (event.ctrlKey || event.metaKey) return "zoom";
  if (event.deltaMode !== 0) return "zoom";
  if (event.deltaX !== 0) return "pan";
  const delta = Math.abs(event.deltaY);
  return delta >= 50 && Number.isInteger(event.deltaY) ? "zoom" : "pan";
}

// How much one wheel event scales the canvas: smooth for small pinch steps,
// about 15% for a mouse notch.
export function wheelFactor(event) {
  const pixels =
    event.deltaY *
    (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1);
  const step = event.ctrlKey ? 0.01 : 0.0015;
  return Math.exp(-pixels * step);
}

// The zoom and scroll offsets after zooming to `next` around `point`
// (viewport pixels from its top-left corner): the scene point under it
// stays under it.
export function zoomAt(view, point, next) {
  const zoom = clampZoom(next);
  const sceneX = (view.scrollLeft + point.x) / view.zoom;
  const sceneY = (view.scrollTop + point.y) / view.zoom;
  return {
    zoom,
    scrollLeft: sceneX * zoom - point.x,
    scrollTop: sceneY * zoom - point.y,
  };
}

// A press that moves no further than this many pixels is a click; past it,
// it is a drag, which pans and never selects.
export const CLICK_SLOP = 3;

export function dragged(start, point, slop = CLICK_SLOP) {
  return Math.hypot(point.x - start.x, point.y - start.y) > slop;
}

const midpoint = ([a, b]) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });
const spread = ([a, b]) => Math.hypot(a.x - b.x, a.y - b.y);

// Two fingers moving from `before` to `after`: the canvas scales by the
// change in their spread around their midpoint, and pans by how far the
// midpoint moved.
export function pinchStep(before, after) {
  const [from, to] = [midpoint(before), midpoint(after)];
  return {
    factor: spread(before) ? spread(after) / spread(before) : 1,
    center: to,
    pan: { x: to.x - from.x, y: to.y - from.y },
  };
}

const ARROWS = {
  ArrowLeft: [-1, 0],
  ArrowRight: [1, 0],
  ArrowUp: [0, -1],
  ArrowDown: [0, 1],
};

// The node an arrow key moves the selection to from `from`: the nearest
// whose centre lies in the arrow's direction, within 45° of it, with
// distance off the axis counting double. Nodes are `{x, y}` centres.
export function neighbour(nodes, from, key) {
  const [ax, ay] = ARROWS[key] ?? [0, 0];
  let best, bestScore = Infinity;
  for (const node of nodes) {
    const dx = node.x - from.x,
      dy = node.y - from.y;
    const along = dx * ax + dy * ay,
      across = Math.abs(ax ? dy : dx);
    if (node === from || along <= 0 || across > along) continue;
    const score = along + 2 * across;
    if (score < bestScore) [best, bestScore] = [node, score];
  }
  return best;
}

// The zoom and scroll offsets that centre scene rectangle `rect` in a
// `viewport`-sized view with `margin` pixels around it, at no more than
// 100% and within the zoom bounds. Offsets are from the scene's corner and
// go negative when the rectangle is smaller than the view.
export function frame(viewport, rect, margin = 80) {
  const zoom = clampZoom(
    Math.min(
      1,
      (viewport.width - 2 * margin) / rect.width,
      (viewport.height - 2 * margin) / rect.height,
    ),
  );
  return {
    zoom,
    scrollLeft: (rect.x + rect.width / 2) * zoom - viewport.width / 2,
    scrollTop: (rect.y + rect.height / 2) * zoom - viewport.height / 2,
  };
}
