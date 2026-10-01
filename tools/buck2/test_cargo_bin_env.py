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
        for features in ([], ["rlm"], ["testing"], ["rlm", "testing", "restate", "sqlite"]):
            with self.subTest(features=features):
                graph = generator.FeatureLaneGraph(metadata, {}, {})
                resolution = feature_variants.resolve_request(
                    graph.workspace, "lash-runtime", default_features=False,
                    requested=features, with_dev=True,
                ).sorted_features()
                target = next(target for target in graph.by_name["lash-runtime"]["targets"]
                              if target["name"] == "seam_proof_dialect")
                label = graph.emit_target("lash-runtime", resolution, target, "test", True, [])
                text = "".join(chunk for _, chunk in graph.chunks["lash-runtime"])
                outputs = {generator.ROOT / "crates/lash/BUCK": text}
                self.assertEqual(generator.reconcile_cargo_bin_env(outputs), [])
                rules = {}
                for node in ast.parse(text).body:
                    args = {arg.arg: ast.literal_eval(arg.value) for arg in node.value.keywords
                            if arg.arg in {"name", "crate_features", "rustc_env", "extra_compile_data"}}
                    rules[args["name"]] = args
                test = rules[label.split(":")[1]]
                env = test.get("rustc_env", {})
                enabled = {"rlm", "testing"} <= set(resolution["lash-runtime"])
                self.assertEqual("CARGO_BIN_EXE_lash-seam-proof-worker" in env, enabled)
                if enabled:
                    label = env["CARGO_BIN_EXE_lash-seam-proof-worker"].removeprefix("$(location :").removesuffix(")")
                    self.assertIn(":" + label, test["extra_compile_data"])
                    self.assertEqual(rules[label]["crate_features"], test["crate_features"])
                    self.assertTrue(label.endswith(test["name"].split("__fv_")[1]))


if __name__ == "__main__":
    unittest.main()
