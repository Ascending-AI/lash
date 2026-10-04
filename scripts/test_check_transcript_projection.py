#!/usr/bin/env python3
"""Red-side fixtures for both scrapes and the renderer boundary."""

from pathlib import Path
import tempfile
import unittest

import check_transcript_projection as gate


class TranscriptProjectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        kinds = sorted(gate.KINDS)
        self.write("crates/lash-core-store/src/transcript/mod.rs", "pub enum TranscriptRowKind {\n" + "\n".join(f"{kind}," for kind in kinds) + "\n}")
        self.write("examples/agent-workbench/tests/transcript_projection_harness.mjs", "export const SURFACES = ['host'];\n")
        self.write("examples/host/asset.html", "// BEGIN ROWS\nfunction render() {}\n// END ROWS\n")
        self.write("examples/host/src/render.rs", "fn render(view: View) { view.transcript(); }\n")
        self.prefix = f"row_kinds = {kinds!r}\n"
        self.surface = f"""
[[surfaces]]
name = 'host'
asset = 'examples/host/asset.html'
sources = ['examples/host/src/render.rs']
begin = '// BEGIN ROWS'
end = '// END ROWS'
harness = 'examples/agent-workbench/tests/transcript_projection_harness.mjs'
row_kinds = {kinds!r}
"""
        self.registry()

    def write(self, relative, source):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source)

    def registry(self, entries=""):
        self.write("scripts/transcript-projection-sites.toml", self.prefix + self.surface + "\n[sites]\n" + entries)

    def errors(self):
        return "\n".join(gate.check(self.root))

    def test_every_rendered_surface_needs_a_harness_row(self):
        self.write("examples/another/assets/index.html", "node.dataset.transcriptRowId = row.row_id;")
        self.assertIn("unregistered rendered surface", self.errors())

    def test_a_registered_surface_must_be_in_the_shared_harness(self):
        self.write("examples/agent-workbench/tests/transcript_projection_harness.mjs", "export const SURFACES = [];\n")
        self.assertIn("registry and shared harness surfaces differ", self.errors())

    def test_reviewed_baseline_passes(self):
        self.assertEqual([], gate.check(self.root))

    def test_new_committed_truth_reader_is_red(self):
        self.write("examples/other/src/main.rs", "fn hidden(view: View) { view.messages(); }")
        self.assertIn("unregistered transcript", self.errors())
        self.assertIn("reads cardinality changed: 1", self.errors())

    def test_single_output_accessor_renderer_is_red(self):
        self.write("examples/other/src/main.rs", "fn hidden(output: Output) { persist(output.assistant_message()); }")
        self.assertIn("unregistered transcript", self.errors())
        self.assertIn("outputs cardinality changed: 1", self.errors())

    def test_a_single_qualified_output_call_is_red(self):
        self.write("examples/other/src/main.rs", "fn hidden(output: Output) { persist(TurnOutput::assistant_message(&output)); }")
        self.assertIn("unregistered transcript", self.errors())
        self.assertIn("outputs cardinality changed: 1", self.errors())

    def test_registered_function_cannot_grow_a_second_selection(self):
        self.write("crates/probe/src/main.rs", "fn probe(output: Output) { assert(output.assistant_message()); output.final_value(); }")
        self.registry('"crates/probe/src/main.rs#probe" = { reads = 0, outputs = 1, kind = "output-read", reason = "typed result assertion" }')
        self.assertIn("scrape counts changed", self.errors())

    def test_evidence_read_cannot_select_output(self):
        self.write("crates/probe/src/main.rs", "fn probe(output: Output) { output.tool_value(); }")
        self.registry('"crates/probe/src/main.rs#probe" = { reads = 0, outputs = 1, kind = "evidence-read", reason = "effect evidence" }')
        self.assertIn("evidence-read cannot select", self.errors())

    def test_own_namespace_id_parser_is_red(self):
        self.write("examples/host/src/render.rs", 'fn render(message: Message) { message.id.strip_prefix("host-owned:"); }')
        self.assertIn("host parses transcript identity", self.errors())

    def test_committed_part_classifier_is_red(self):
        self.write("examples/host/src/render.rs", "fn render(part: Part) { if part.kind() == PartKind::Reasoning {} }")
        self.assertIn("host re-derives committed classification", self.errors())

    def test_new_kind_requires_every_surface_to_handle_it(self):
        self.write("crates/lash-core-store/src/transcript/mod.rs", "pub enum TranscriptRowKind {\nSecret,\n}")
        self.assertIn("row kind coverage changed", self.errors())

    def test_stored_cardinality_totals_are_red(self):
        self.write("scripts/transcript-projection-sites.toml", self.prefix + "[cardinality]\nreads = 0\noutputs = 0\n" + self.surface + "\n[sites]\n")
        self.assertIn("derived from [sites], not stored", self.errors())

    def test_deleted_harness_marker_is_red(self):
        self.write("examples/host/asset.html", "function render() {}")
        self.assertIn("renderer marker missing", self.errors())

    def test_comments_and_literals_do_not_create_sites(self):
        self.write("examples/host/src/render.rs", 'fn render() { let fixture = r###"value.final_value()"###; }\n// view.messages()\n/* view.message_tree() */')
        self.assertEqual([], gate.check(self.root))


if __name__ == "__main__":
    unittest.main()
