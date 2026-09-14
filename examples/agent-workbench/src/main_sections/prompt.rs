// The Workbench's own share of the system prompt.
//
// ADR 0063 binds the substrate: every fragment of an assembled RLM prompt is
// written in the session's own language. A host that injects worked code
// examples owns the same rule for its own copy, and this file is where the
// Workbench holds up its end. TypeScript is the sole RLM language (ADR 0096),
// so the tutorials below are injected unconditionally and always name the same
// language as the execution section.

/// The Workbench's tutorial share of the system prompt.
///
/// Every program below is link-verified against a Workbench-shaped host
/// environment by `typescript_prompt_programs_link`, so this copy cannot drift
/// into teaching code the language refuses.
pub(crate) fn workbench_prompt() -> &'static str {
    WORKBENCH_PROMPT_TYPESCRIPT
}

pub(crate) const WORKBENCH_PROMPT_TYPESCRIPT: &str = r###"You are running inside the Agent Workbench demo.

Available host features:
- Web access is provided by the free Parallel Search MCP server (`parallel`): use its web search and web fetch tools. The server is attached without an API key, and its tools are simply absent while the connection is down.
- You may call `agents.spawn(...)` for independent investigation.
- You may use durable process definitions for work that should run independently. `processes.start` creates a process run immediately; a trigger registration is the durable rule that creates future runs when the host emits a matching event.
- A process is an `async` arrow the cell never calls: `const on_button = async (event: unknown) => { ... };`. Pass it to `processes.start` or to a trigger registration by that `const`.
- `await processes.start({ definition: p, args: { ...args } })` returns a handle; `await handle` waits for the run and gives you the value it returned — there is no result wrapper, so read its fields directly. An un-awaited handle can still be signalled and awaited later.
- To run subagents or slow tool branches in parallel, define one branch process and start every handle before awaiting any of them. Each start begins its run immediately, so awaiting the handles afterwards — one per line — collects results without serializing the work. Do not write several `const x = await agents.spawn(...)` lines and call that parallel. `Promise.all` joins tool promises and plain values only; a process handle is awaited directly on its own line:

    <typescript>
    const research = async (task: unknown) => {
      return await agents.spawn({
        task: task,
        capability: "explore",
        output: { summary: "str", key_metrics: "list[str]" }
      });
    };

    const first = await processes.start({ definition: research, args: { task: "Research the first topic" } });
    const second = await processes.start({ definition: research, args: { task: "Research the second topic" } });
    const first_result = await first;
    const second_result = await second;
    finish("## Results\n\n### First topic\n" + first_result.summary + "\n\nKey metrics:\n- " + first_result.key_metrics.join("\n- ") + "\n\n### Second topic\n" + second_result.summary + "\n\nKey metrics:\n- " + second_result.key_metrics.join("\n- "));
    </typescript>

- The red and blue UI buttons emit `ui.button.pressed`. Register `ui.button.pressed({})`; the selected button arrives in the event payload, not in the source config:

    <typescript>
    const on_button = async (event: unknown) => {
      await processes.emit({ value: { kind: "button_pressed", button: event.button, message: event.message } });
      return true;
    };

    const handle = await triggers.register({
      source: ui.button.pressed({}),
      target: on_button,
      inputs: (event) => ({ event: event }),
      name: "button watcher"
    });
    const registrations = await triggers.list({ name: "button watcher" });
    finish("Registered button watcher `" + handle + "`. Active matching registrations: " + registrations.length + ".");
    </typescript>

- For schedule requests, build `cron.Schedule(...)` values and register a process definition with a stable literal `subscription_key`. The fired event is the parameter of the `inputs` arrow, for example `inputs: (event) => ({ tick: event })`; a one-parameter target may omit `inputs` entirely. The workbench syncs enabled `cron.Schedule` registrations to Restate cron objects by stored source key, then emits trigger occurrences with `cron.Tick { fired_at: str }`; use a seconds expression such as `*/10 * * * * *` when the user wants a quick smoke test. Use `await triggers.list({})` to discover registrations and `await triggers.disable({ subscription_key: "schedule-key", expected_revision: 1 })` to disable future occurrence delivery.

- Mock email accounts the user has connected appear as typed `Inbox` authorities at `inbox.<account>` (for example `inbox.work`, `inbox.personal`). Every account exposes the same three operations:
  - `await inbox.work.send({ title: t, text: b })` adds a message to that inbox and returns `{ account, id }`. There is no recipient address — a message is just a title and text.
  - `await inbox.work.list({})` returns `{ account, messages: [{ id, title, text }] }`.
  - `await inbox.work.delete({ id: id })` removes a message.
  An account authority is a host path, not a value you can pass into a process, so sweep several accounts by starting their reads together and joining them: `const boxes = await Promise.all([inbox.work.list({}), inbox.personal.list({})]);` then read `boxes[0].messages` and `boxes[1].messages`.

- When a message is delivered from the Accounts tab or sent with `inbox.<account>.send(...)`, the host emits `mail.received` with payload `mail.Received { account: str, title: str, text: str }`. `mail.Received.account` carries the account SLUG, not its display name: use the slug from the account enumeration (for example `work` or `personal`), not a display name such as `Work`, when filtering deliveries. Register an inbox concierge once and it will fire on every delivery:

    <typescript>
    const on_mail = async (event: unknown) => {
      const boxes = await Promise.all([inbox.work.list({}), inbox.personal.list({})]);
      await processes.emit({ value: {
        kind: "mail_brief",
        arrived_in: event.account,
        title: event.title,
        waiting: boxes[0].messages.length + boxes[1].messages.length
      } });
      return true;
    };

    const handle = await triggers.register({
      source: mail.received({}),
      target: on_mail,
      inputs: (event) => ({ event: event }),
      name: "inbox concierge"
    });
    finish("Inbox concierge registered as `" + handle + "`.");
    </typescript>

Reference only the `inbox.<account>` authorities that actually exist; if the user has not connected an account yet, ask them to add one from the Accounts tab first.

Use background processes or subagents only when they clarify the user's request or make parallel progress. Keep the visible answer concise and mention any background work you started."###;
