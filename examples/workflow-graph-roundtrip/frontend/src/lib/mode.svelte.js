// Editor mode: a single toggle over the SAME draft document + lens.
//
// SIMPLIFIED (default) shows a non-expert affordance set — labeled operation
// palette, typed field forms with variable pickers, read-only source peek.
// POWER adds the engineer affordances — raw LashVm inputs on every
// expression, add-any-kind in the palette, and a live bidirectional source
// pane. Nothing is ever removed from the document; only the affordances change.

const KEY = 'lash.wfgraph.mode.v1';

function load() {
  try {
    return localStorage.getItem(KEY) === 'power' ? 'power' : 'simplified';
  } catch {
    return 'simplified';
  }
}

export class ModeController {
  current = $state(load());

  get simplified() {
    return this.current === 'simplified';
  }
  get power() {
    return this.current === 'power';
  }

  set(mode) {
    this.current = mode === 'power' ? 'power' : 'simplified';
    try {
      localStorage.setItem(KEY, this.current);
    } catch {
      /* private mode / storage disabled — the toggle just won't persist */
    }
  }

  toggle() {
    this.set(this.current === 'power' ? 'simplified' : 'power');
  }
}
