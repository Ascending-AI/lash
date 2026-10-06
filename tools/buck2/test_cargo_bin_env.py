"""Cargo binary inputs must exist in the selected feature resolution."""

import ast
import unittest

import feature_variants
import generate_model as generator
import sync


class CargoBinEnvTests(unittest.TestCase):
    def test_checked_in_binary_inputs_exist(self):
        outputs = {path: path.read_text() for path in generator.ROOT.glob("*/*/BUCK")}
        self.assertEqual(generator.reconcile_cargo_bin_env(outputs), [])

    def test_census_rejects_dangling_binary_inputs(self):
        path = generator.ROOT / "crates/lash/BUCK"
        target = 'lash_rust_feature_test(name="test__fv_fixture", rustc_env={"CARGO_BIN_EXE_worker": "$(location :worker__bin__fv_fixture)"}, extra_compile_data=[":worker__bin__fv_fixture"])'
        self.assertEqual(len(generator.reconcile_cargo_bin_env({path: target})), 1)
        worker = '\nlash_rust_feature_binary(name="worker__bin__fv_fixture")'
        self.assertEqual(generator.reconcile_cargo_bin_env({path: target + worker}), [])

    def test_test_emits_only_enabled_binary_dependencies(self):
        metadata = sync.metadata()
        for features in ([], ["testing"], ["synthetic-next", "testing"]):
            with self.subTest(features=features):
                graph = generator.FeatureLaneGraph(metadata, {}, {})
                resolved = feature_variants.resolve_request(
                    graph.workspace, "lash-internal-vm-worker", default_features=False,
                    requested=features, with_dev=True,
                )
                resolution = resolved.sorted_features()
                # `FeatureLaneGraph.build` records each command's activations
                # before it emits the command's targets.
                graph.record_activations(resolved)
                target = next(target for target in graph.by_name["lash-internal-vm-worker"]["targets"]
                              if target["name"] == "pool_laws")
                label = graph.emit_target("lash-internal-vm-worker", resolution, target, "test", True, [])
                text = "".join(chunk for _, chunk in graph.chunks["lash-internal-vm-worker"])
                outputs = {generator.ROOT / "crates/lash-vm-worker/BUCK": text}
                self.assertEqual(generator.reconcile_cargo_bin_env(outputs), [])
                rules = {}
                for node in ast.parse(text).body:
                    args = {arg.arg: ast.literal_eval(arg.value) for arg in node.value.keywords
                            if arg.arg in {"name", "crate_features", "rustc_env", "extra_compile_data"}}
                    rules[args["name"]] = args
                test = rules[label.split(":")[1]]
                env = test.get("rustc_env", {})
                enabled = "testing" in set(resolution["lash-internal-vm-worker"])
                self.assertEqual("CARGO_BIN_EXE_lash-vm-worker-fixture" in env, enabled)
                if enabled:
                    label = env["CARGO_BIN_EXE_lash-vm-worker-fixture"].removeprefix("$(location :").removesuffix(")")
                    self.assertIn(":" + label, test["extra_compile_data"])
                    self.assertEqual(rules[label]["crate_features"], test["crate_features"])
                    self.assertTrue(label.endswith(test["name"].split("__fv_")[1]))


if __name__ == "__main__":
    unittest.main()
