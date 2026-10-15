// What the user has picked out on the canvas or in the code pane, and
// whether its inspector is open. A click only selects; opening is explicit:
// a double-click, Enter, or the selected item's Open control. Escape closes
// an open inspector before it clears the selection. Items are string keys.

export const NOTHING = Object.freeze({ selected: undefined, open: false });

// A click on `item`: it becomes the selection with its inspector closed. A
// click on the selection itself changes nothing, so an open inspector stays.
export function select(state, item) {
  if (item === undefined) return NOTHING;
  return item === state.selected ? state : { selected: item, open: false };
}

// Open `item`'s inspector, selecting it.
export function open(state, item = state.selected) {
  return { selected: item, open: true };
}

// Close the inspector and keep the selection.
export function close(state) {
  return state.open ? { ...state, open: false } : state;
}

export function escape(state) {
  return state.open ? close(state) : NOTHING;
}
