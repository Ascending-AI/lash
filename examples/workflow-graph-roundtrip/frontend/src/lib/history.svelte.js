// Bounded undo/redo history over draft-document snapshots.
//
// The draft is the single source of truth (App owns it, node components mutate
// it live). History captures whole-document snapshots at commit points —
// adopting a workflow seeds a baseline, and every add/delete/reorder/move plus
// each committed field edit pushes a new snapshot. Undo/redo hand a clone back
// to App, which re-adopts it. Snapshots are deep clones, so an undone edit can
// never alias the live draft.

const LIMIT = 200;
// `doc` may be a Svelte 5 $state proxy (draftDoc), which structuredClone cannot
// clone ("#<Object> could not be cloned"). Snapshot to a plain object first.
const clone = (doc) => structuredClone($state.snapshot(doc));

export class History {
  // The stack itself is not reactive (it is only read on undo/redo); `index`
  // and `size` are reactive so `canUndo`/`canRedo` drive button state.
  #stack = [];
  index = $state(-1);
  size = $state(0);

  reset(doc) {
    this.#stack = doc ? [clone(doc)] : [];
    this.index = this.#stack.length - 1;
    this.size = this.#stack.length;
  }

  commit(doc) {
    if (!doc) return;
    if (this.index < this.#stack.length - 1) {
      this.#stack = this.#stack.slice(0, this.index + 1);
    }
    this.#stack.push(clone(doc));
    if (this.#stack.length > LIMIT) this.#stack.shift();
    this.index = this.#stack.length - 1;
    this.size = this.#stack.length;
  }

  get canUndo() {
    return this.index > 0;
  }
  get canRedo() {
    return this.index < this.size - 1;
  }

  undo() {
    if (!this.canUndo) return null;
    this.index -= 1;
    return clone(this.#stack[this.index]);
  }

  redo() {
    if (!this.canRedo) return null;
    this.index += 1;
    return clone(this.#stack[this.index]);
  }
}
