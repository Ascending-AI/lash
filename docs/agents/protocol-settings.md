# Protocol presentation presets

D-DEFAULTS2 keeps presentation choices optional. Every value below is a host
setting through the `lash` facade. The inherited numbers are documented
presets, without workload measurements establishing universal defaults.
Execution spend and output retention remain separate host decisions.
Immutable refusal ceilings remain unchanged.

| Family | Facade setting and standard preset |
| --- | --- |
| Standard protocol | `plugins::StandardProtocolConfig::standard()`; no discovery, built-in renderer, batch enabled with 64 members |
| Standard tool output | `render::ToolRenderParams::standard()`; 16,000 characters, 400 lines, 50 percent head, prefer authored view; standard value render |
| Value render | `rlm::RenderParams::standard()` and `rlm::RenderParams::preview()` supply the protocol's print/preview patches; standard print is 8,000 characters, auto format, width 80, indent 2, depth 3, array threshold 10, head 3, tail 2, minimum cut 80, stack head 3 and tail 1; preview is compact, 1,000 characters and depth 2 |
| RLM protocol | `rlm::RlmProtocolPluginConfig::standard()` starts the typed builder; images and decomposition on, durable sleep always available, label annotations on, 10,000 output characters, warning at 100,000 tokens, no discovery; execution budgets are explicit inputs, and `rlm::InstructionBound::standard()` is the instruction budget measured over the repository's model-written cells, 20,000,000 charge units. Discovery (`with_discovery`) has no preset: an `rlm::RlmDiscovery` names the discovery tool and the bytes the listed tool docs share |
| RLM presentation | `rlm::RlmPresentationConfig::standard()`; 12 inline object keys, 128 host-call records and diagnostic ledger entries, a cell's listed calls cut by `rlm::CallFeedbackPolicy::standard()` (512-byte result summaries in a 16 KiB block, at most 256 KiB, recorded at session creation), 64 KiB scalar bodies, 32 binding-change names per cell record, 24 KiB of bound-variable rows and their schema definitions |
| Tool presentation | Each mode's tool presenter owns its config and admits a host's `tool_presentation` at creation; the session records the presenter's id and complete admitted config. The shipped TypeScript, Python and standard presenters take `{}` and import complete schemas under a 10,000-node budget; exhaustion refuses with `shape.node_limit`. Replace or wrap a presenter with `rlm::CellDialect::with_tool_presenter`, `StandardProtocolConfig::tool_presenter` and `tools::WrappedToolPresenter`. |
| Catalogue advertisement | `tools::CataloguePreviewOptions::standard()`; title Catalogued Capabilities, search through tools.search, 100 modules and 50 call names |
| Prompt composition | `prompt::PromptLimits::standard()` on the recorded prompt plan: 128 sections, 256 wrappers, 32 KiB per section, 256 KiB total, 2,000 ms including queue time |
| Prompt render pool | `plugins::PromptRenderPoolConfig::standard()`; available parallelism clamped to one through eight workers, queue 1,024; select capacity through `LashCoreBuilder::prompt_render_pool` |
| Runtime transcript | `RuntimeOutputCuts::standard()`; 16,384 value-reply characters and 4,000 raw-error characters plus omission markers; install through `LashCoreBuilder::output_cuts` |
| Compaction | `plugins::StandardCompactionConfig::standard()`; pressure/pruning/recovery on, buffer and explicit-compaction eligibility 20,000 tokens, zero recent turns copied, two recent user turns protected from pruning, pruning at 60 percent, four bytes per estimated token, 1,200 tokens per attachment, 512 tokens overhead, three recovery attempts, elide at 16,000 tokens retaining 400 characters |

Compaction instruction strings are configurable. The standard summary and
update prompts use Goal, Instructions, Discoveries, Accomplished and Files
sections and preserve the prior summary. The update template replaces
`{previous_summary}`. The recovery prompt summarizes unfinished work without
rerunning a tool. `retained_user_turns` copies complete user turns into the
fresh frame alongside the summary; their original durable history remains.

An empty render patch means inherit; it is not a base preset. Creation records
a fully resolved standard or RLM base, including RLM presentation. Subsequent
opens use the recorded behaviour. A recorded RLM namespace missing presentation
is refused rather than assigned the reopening host's values. Durable `sleep`
and `sleep_until` are always available to cells and processes. A sleep parks
on a timer row in the waits table; turn deadlines and host cancellation bound
it like any parked call.
RLM uses one transcript projection policy for its native call/result exchanges.

A discovery session lists the discovery tool and the control tools, then
the inline tools that fit its recorded catalogue budget, one namespace at a
time in turn, in catalogue order under each module's heading. A label after
the docs says whether the listing is complete, and how many tools the
discovery tool finds beyond it. A tool appended to the catalogue appends
its doc, so the listing before it keeps its bytes; a removal rewrites the
listing from the removed tool on. `rlm::tool_catalogue_disclosure` gives a
host the same facts, and `rlm::discovered_tool_doc` and
`rlm::discovery_text` show and index a tool its discovery finds, field
notes included.

The bound-variables section lists the session's globals in name order
until the presentation budget is spent, and counts the rest. A saved
function shows the cell its captures froze at; no global names the cell
that last bound it. A host handle is listed
as opaque. A host reads every global, listed or not, from the call's
`rlm::RlmPromptFacts`, and pages the omitted ones with the snapshot-scoped
`rlm::GlobalsPage` it leaves.
