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
| RLM protocol | `rlm::RlmProtocolPluginConfig::standard()` starts the typed builder; images and decomposition on, sleep off, label annotations on, 10,000 output characters, warning at 100,000 tokens, no discovery; channel and execution budgets are explicit inputs |
| RLM presentation | `rlm::RlmPresentationConfig::standard()`; 12 inline object keys, 128 host-call records and diagnostic ledger entries, 64 KiB scalar bodies |
| Compact contracts | `tools::ToolPresentationConfig::standard()`; two examples, 240 characters each, schema depth eight; use `ToolContract::compact_contract_with_presentation` for a custom policy |
| Heap summaries | `rlm::lang::BindingSummaryConfig::standard()`; four members, depth two, 160 characters; RLM carries custom values to the VM worker through its presentation config |
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
is refused rather than assigned the reopening host's values. An authored
sleep opt-out is preserved even on a deployment with process lifecycle.
Both RLM channels use one shared transcript projection policy.
