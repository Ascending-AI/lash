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
    scrollLeft: Math.max(0, sceneX * zoom - point.x),
    scrollTop: Math.max(0, sceneY * zoom - point.y),
  };
}

// The zoom that shows a whole `graph` inside `viewport`, leaving `margin`
// pixels free; never larger than 100%.
export function fitZoom(viewport, graph, margin = 60) {
  return clampZoom(
    Math.min(
      1,
      (viewport.width - margin) / graph.width,
      (viewport.height - margin) / graph.height,
    ),
  );
}
