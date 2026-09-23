import { runWorkflow } from './api.js';
import { acceptsRunEvent } from './runEvents.js';

const EMPTY_DISPLAY = {
  messages: [],
  statuses: {},
  lists: {},
  lights: {},
  progress: 0,
  highlighted: null,
};

// Holds everything about the CURRENT run (invocation): which node is lit and
// with what status, the accumulating display panel state, and lifecycle flags.
// Each Play starts a fresh run; a new run resets the overlay before streaming.
export class RunController {
  running = $state(false);
  runId = $state(null);
  workflowVersion = $state(null);
  // The loaded document's `definition`: the one artifact this overlay shows.
  definition = $state(null);
  discarded = $state(0);
  // nodeId -> 'started' | 'running' | 'waiting' | 'succeeded' | 'failed'
  overlay = $state({});
  activeNodeId = $state(null);
  display = $state({ ...EMPTY_DISPLAY });
  eventCount = $state(0);
  error = $state(null);
  finished = $state(false);

  #abort = null;

  reset() {
    this.overlay = {};
    this.activeNodeId = null;
    this.display = { ...EMPTY_DISPLAY };
    this.eventCount = 0;
    this.error = null;
    this.finished = false;
  }

  async start(definition) {
    // A new Play supersedes any in-flight run.
    if (this.#abort) this.#abort.abort();
    this.reset();
    this.running = true;
    this.runId = null;
    this.definition = definition ?? null;
    this.discarded = 0;
    const controller = new AbortController();
    this.#abort = controller;

    try {
      for await (const ev of runWorkflow(controller.signal)) {
        if (controller.signal.aborted) return;
        this.#apply(ev);
      }
      this.finished = true;
    } catch (err) {
      if (!controller.signal.aborted) {
        this.error = err?.message ?? String(err);
      }
    } finally {
      if (this.#abort === controller) {
        this.running = false;
        this.#abort = null;
      }
    }
  }

  stop() {
    if (this.#abort) this.#abort.abort();
    this.running = false;
  }

  #apply(ev) {
    if (!acceptsRunEvent(this.definition, ev)) {
      this.discarded += 1;
      return;
    }
    this.runId = ev.runId;
    this.workflowVersion = ev.workflowVersion;
    this.eventCount += 1;
    if (ev.display) {
      this.display = {
        messages: ev.display.messages ?? [],
        statuses: ev.display.statuses ?? {},
        lists: ev.display.lists ?? {},
        lights: ev.display.lights ?? {},
        progress: ev.display.progress ?? 0,
        highlighted: ev.display.highlighted ?? null,
      };
    }

    const next = { ...this.overlay };
    // A `started` becomes a steady "running" glow until its terminal event.
    const status = ev.status === 'started' ? 'running' : ev.status;
    next[ev.nodeId] = status;
    this.overlay = next;

    if (ev.status === 'started' || ev.status === 'waiting') {
      this.activeNodeId = ev.nodeId;
    } else if (this.activeNodeId === ev.nodeId && ev.status === 'succeeded') {
      // keep it as the most-recently-active for a beat; the next started moves it
    }
    if (ev.status === 'failed') {
      this.error = ev.error ?? 'run failed';
    }
  }
}
