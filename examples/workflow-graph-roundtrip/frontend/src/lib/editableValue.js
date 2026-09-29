/** @typedef {import('../generated/workflow-document').EditableValue} EditableValue */

/** @param {EditableValue | undefined} editable */
export function editableKind(editable) {
  return editable?.kind;
}

// Only literal values are converted to ordinary JSON. Expression text stays
// inside its tagged wrapper when nested in a record or list.
export function literalValue(editable) {
  switch (editableKind(editable)) {
    case 'null': return null;
    case 'list': return editable.value.map(literalValue);
    case 'object': return Object.fromEntries(
      Object.entries(editable.value).map(([key, value]) => [key, literalValue(value)]),
    );
    case 'expr': return editable;
    default: return editable?.value;
  }
}

export function editableText(editable) {
  const kind = editableKind(editable);
  if (kind === 'null' || kind === undefined) return '';
  if (kind === 'object' || kind === 'list') return JSON.stringify(literalValue(editable));
  return String(editable.value);
}

// Build source recursively; literal keys, including $expr, are always quoted.
export function editableSource(editable) {
  switch (editableKind(editable)) {
    case 'expr': return editable.value;
    case 'list': return `[${editable.value.map(editableSource).join(', ')}]`;
    case 'object': return `{ ${Object.entries(editable.value)
      .map(([key, value]) => `${JSON.stringify(key)}: ${editableSource(value)}`).join(', ')} }`;
    default: return JSON.stringify(literalValue(editable));
  }
}
