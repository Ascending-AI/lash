# Retired: RLM text-cell boundary

The text-cell parser, stream mask and suffix-discard boundary were removed by
FIG-7374. [ADR 0148](../../docs/adr/0148-rlm-uses-one-provider-native-code-channel.md)
requires every RLM program to use a complete provider-native `execute_code` call.
The former live scenario and its parser laws no longer apply.
