// A run event belongs to the overlay only when it comes from the admitted
// artifact the loaded graph is the view of: the event's `definition` is the
// document's. An event from any other definition (a foreign or stale graph)
// is discarded rather than lighting nodes that merely share an id.

/**
 * @param {string | null | undefined} expectedDefinition the loaded document's `definition`
 * @param {{ definition?: string }} event a run event
 * @returns {boolean}
 */
export function acceptsRunEvent(expectedDefinition, event) {
  return (
    typeof expectedDefinition === 'string' &&
    expectedDefinition.length > 0 &&
    event?.definition === expectedDefinition
  );
}
