// Labels for the slots of a node's typed statement, as the structured editor
// lists them. A slot path is a list of typed segments (`{ expr: "value" }`,
// `{ expr: { arg: 0 } }`, ...); the label spells it and names the IR variant
// it holds.

function segmentText(segment) {
  const slot = segment?.expr ?? segment;
  if (typeof slot === 'string') return slot;
  const [name, index] = Object.entries(slot ?? {})[0] ?? ['?', ''];
  return `${name} ${index}`;
}

export function slotPathText(path) {
  return (path ?? []).map(segmentText).join(' › ');
}

export function slotLabel(slot) {
  return `${slotPathText(slot.path)} — ${slot.variant}`;
}
