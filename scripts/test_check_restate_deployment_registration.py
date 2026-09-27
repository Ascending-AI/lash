#!/usr/bin/env python3
"""Tests for scripts/check_restate_deployment_registration.py.

Every positive test flags one way a raw `POST .../deployments` can come back:
the inline reqwest post the hosts used to make, the multi-line curl form the
agent-workbench launcher used to make, and a POST against a URL assembled a
line or two earlier. The negative tests are the legal neighbours the gate
must not eat: the read-only `GET /deployments` listing, the launcher's
`register-deployment` subcommand dispatch, and a POST to an unrelated path
near the word deployments. The repository itself is verified last, so the
rule is proven narrow enough for the shape the tree actually uses.
"""

from __future__ import annotations

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_restate_deployment_registration as checker  # noqa: E402


REQWEST_POST = """\
let response = reqwest::Client::new()
    .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
    .json(&serde_json::json!({ "uri": uri, "force": true }))
    .send()
    .await?;
"""

MULTILINE_POST = """\
let response = client
    .post(
        format!("{}/deployments", admin_url.trim_end_matches('/'))
            .as_str(),
    )
    .send()
    .await?;
"""

CURL_POST = """\
register() {
  curl --http2-prior-knowledge -fsS \\
    -H 'content-type: application/json' \\
    -X POST \\
    --data "$payload" \\
    "${admin_url%/}/deployments"
}
"""

URL_BUILT_EARLIER = """\
let url = format!("{}/deployments", admin_url.trim_end_matches('/'));
let body = json!({ "uri": endpoint_url });
let response = send_request(
    &connection,
    HttpRequest::post(&url, body),
)
.await?;
"""

PYTHON_POST = """\
import requests
requests.post(f"{admin_url}/deployments", json={"uri": uri})
"""


class ViolationTests(unittest.TestCase):
    def assert_flags(self, text: str) -> None:
        self.assertTrue(
            checker.find_violations(text),
            f"expected a raw registration violation in:\n{text}",
        )

    def assert_clean(self, text: str) -> None:
        self.assertEqual(
            checker.find_violations(text),
            [],
            f"read-only or guarded access must not be flagged:\n{text}",
        )

    def test_inline_reqwest_post_is_flagged(self) -> None:
        self.assert_flags(REQWEST_POST)

    def test_multiline_post_is_flagged(self) -> None:
        self.assert_flags(MULTILINE_POST)

    def test_curl_post_is_flagged(self) -> None:
        self.assert_flags(CURL_POST)

    def test_url_built_before_the_post_is_flagged(self) -> None:
        self.assert_flags(URL_BUILT_EARLIER)

    def test_python_post_is_flagged(self) -> None:
        self.assert_flags(PYTHON_POST)


class LegalNeighbourTests(unittest.TestCase):
    def assert_clean(self, text: str) -> None:
        self.assertEqual(
            checker.find_violations(text),
            [],
            f"read-only or guarded access must not be flagged:\n{text}",
        )

    def test_get_deployments_listing_is_legal(self) -> None:
        self.assert_clean(
            'let registry = client.get(format!("{}/deployments", admin_url)).send().await?;'
        )

    def test_curl_get_deployments_is_legal(self) -> None:
        self.assert_clean(
            'curl --http2-prior-knowledge -fsS "${admin_url%/}/deployments"'
        )

    def test_subcommand_dispatch_is_legal(self) -> None:
        self.assert_clean(
            '"$workbench_bin" register-deployment "$endpoint_url"'
        )

    def test_post_to_another_path_is_legal(self) -> None:
        self.assert_clean(
            "let response = client\n"
            '    .post(format!("{ingress}/{service}/{key}/send"))\n'
            "    .send()\n"
            "    .await?;\n"
        )


class RepositoryTest(unittest.TestCase):
    def test_repository_has_no_raw_registration(self) -> None:
        checker.verify()


if __name__ == "__main__":
    unittest.main()
