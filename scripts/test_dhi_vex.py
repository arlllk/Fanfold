"""Unit tests for dhi_vex.py (offline): python3 -I -B -m unittest discover -s scripts"""

from __future__ import annotations

import json
import os
import tempfile
import unittest
from unittest import mock

import dhi_vex as d

DIGEST = "sha256:" + "c" * 64
BASE = "dhi.io/python:3.15.2-debian13@" + DIGEST


def spdx(*purls: str) -> dict:
    return {"packages": [{"externalRefs": [{"referenceType": "purl", "referenceLocator": p}]} for p in purls]}


def statement(cve: str, *products: str, status: str = "not_affected") -> dict:
    return {
        "vulnerability": {"name": cve},
        "status": status,
        "justification": "vulnerable_code_not_present",
        "products": [{"@id": p} for p in products]
        + [{"@id": "https://dhi.io/v2/python/manifests/" + DIGEST}],  # DHI's image: never a purl
    }


SBOM = spdx(
    "pkg:deb/debian/libssl3t64@3.5.7-1~deb13u3%2Bdhi2?arch=amd64&os_name=debian&upstream=openssl",
    "pkg:deb/debian/libssl3t64@3.5.7-1~deb13u3%2Bdhi2?arch=amd64&os_name=debian&upstream=openssl",
    "pkg:deb/debian/openssl@3.5.7-1~deb13u3%2Bdhi2?arch=amd64&os_name=debian",
    "pkg:deb/debian/libc6@2.41-12%2Bdeb13u4%2Bdhi1?arch=amd64&upstream=glibc",
    # a binary whose version differs from its source's: upstream carries the source version
    "pkg:deb/debian/libgcc-s1@14.2.0-19?arch=amd64&upstream=gcc-14%4014.2.0-19%2Bdhi9",
    "pkg:pypi/setuptools@80.9.0",
)
OPENSSL = "pkg:deb/debian/openssl@3.5.7-1~deb13u3%2Bdhi2?arch=amd64&os_distro=trixie"


class PurlTest(unittest.TestCase):
    def test_a_binary_package_knows_its_source(self):
        purl = d.parse_purl("pkg:deb/debian/libssl3t64@3.5.7-1~deb13u3%2Bdhi2?arch=amd64&upstream=openssl")
        assert purl is not None
        self.assertEqual(purl.source, ("deb", "openssl", "3.5.7-1~deb13u3+dhi2"))
        self.assertEqual(purl.bare, "pkg:deb/debian/libssl3t64@3.5.7-1~deb13u3%2Bdhi2")

    def test_an_upstream_with_its_own_version(self):
        purl = d.parse_purl("pkg:deb/debian/libgcc-s1@14.2.0-19?upstream=gcc-14%4014.2.0-19%2Bdhi9")
        assert purl is not None
        self.assertEqual(purl.source, ("deb", "gcc-14", "14.2.0-19+dhi9"))

    def test_what_is_not_a_purl(self):
        for text in ("https://dhi.io/v2/python/manifests/x", "pkg:deb/debian/noversion", "", "pkg:"):
            self.assertIsNone(d.parse_purl(text), text)


class RestateTest(unittest.TestCase):
    def restated(self, *statements: dict) -> tuple[list[dict], int]:
        document, unmatched = d.restate({"@id": "https://scout/x", "statements": list(statements)}, SBOM, BASE)
        return document["statements"], unmatched

    def test_a_source_package_becomes_every_binary_built_from_it(self):
        [s], unmatched = self.restated(statement("CVE-1", OPENSSL))
        self.assertEqual(unmatched, 0)
        self.assertEqual([p["@id"] for p in s["products"]], [
            "pkg:deb/debian/libssl3t64@3.5.7-1~deb13u3%2Bdhi2",
            "pkg:deb/debian/openssl@3.5.7-1~deb13u3%2Bdhi2",
        ])
        self.assertEqual((s["vulnerability"], s["status"], s["justification"]),
                         ({"name": "CVE-1"}, "not_affected", "vulnerable_code_not_present"))

    def test_sources_with_their_own_version_and_other_ecosystems(self):
        gcc = "pkg:deb/debian/gcc-14@14.2.0-19%2Bdhi9?arch=amd64"
        statements, _ = self.restated(statement("CVE-2", gcc), statement("CVE-3", "pkg:pypi/setuptools@80.9.0"))
        self.assertEqual([[p["@id"] for p in s["products"]] for s in statements],
                         [["pkg:deb/debian/libgcc-s1@14.2.0-19"], ["pkg:pypi/setuptools@80.9.0"]])

    def test_another_version_matches_nothing(self):
        statements, unmatched = self.restated(statement("CVE-4", OPENSSL.replace("dhi2", "dhi3")))
        self.assertEqual((statements, unmatched), ([], 1))

    def test_only_statements_that_hide_a_finding_are_restated(self):
        statements, unmatched = self.restated(
            statement("CVE-5", OPENSSL, status="affected"),
            statement("CVE-6", OPENSSL, status="under_investigation"),
            statement("CVE-7", OPENSSL, status="fixed"),
        )
        self.assertEqual([s["vulnerability"]["name"] for s in statements], ["CVE-7"])
        self.assertEqual(unmatched, 0, "a status that is skipped is not 'unmatched'")

    def test_the_document_is_deterministic(self):
        a = d.restate({"statements": [statement("CVE-9", OPENSSL), statement("CVE-8", OPENSSL)]}, SBOM, BASE)
        b = d.restate({"statements": [statement("CVE-8", OPENSSL), statement("CVE-9", OPENSSL)]}, SBOM, BASE)
        self.assertEqual(json.dumps(a, sort_keys=True), json.dumps(b, sort_keys=True))
        self.assertEqual([s["vulnerability"]["name"] for s in a[0]["statements"]], ["CVE-8", "CVE-9"])


class BaseImageTest(unittest.TestCase):
    def test_the_python_base_stage_is_read(self):
        self.assertEqual(d.base_image("FROM rust:1 AS chef\nFROM %s AS python-base\n" % BASE), BASE)

    def test_a_base_from_elsewhere_has_no_dhi_vex(self):
        self.assertIsNone(d.base_image("FROM gcr.io/distroless/python3-debian13@%s AS python-base" % DIGEST))

    def test_an_unpinned_or_missing_base_is_an_error(self):
        with self.assertRaises(d.Failure):
            d.base_image("FROM dhi.io/python:3.15.2-debian13 AS python-base")
        with self.assertRaises(d.Failure):
            d.base_image("FROM dhi.io/python@%s AS runtime" % DIGEST)


class MainTest(unittest.TestCase):
    def test_a_release_before_dhi_gets_an_empty_document(self):
        with tempfile.TemporaryDirectory() as tmp:
            dockerfile, out = os.path.join(tmp, "Dockerfile"), os.path.join(tmp, "vex.json")
            with open(dockerfile, "w") as handle:
                handle.write("FROM gcr.io/distroless/python3-debian13@%s AS python-base\n" % DIGEST)
            self.assertEqual(d.main(["--dockerfile", dockerfile, "--output", out]), 0)
            with open(out) as handle:
                self.assertEqual(json.load(handle)["statements"], [])

    def test_missing_credentials_fail_instead_of_writing_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            dockerfile, out = os.path.join(tmp, "Dockerfile"), os.path.join(tmp, "vex.json")
            with open(dockerfile, "w") as handle:
                handle.write("FROM %s AS python-base\n" % BASE)
            env = {k: v for k, v in os.environ.items() if k not in ("DHI_USERNAME", "DHI_TOKEN")}
            with mock.patch.dict(os.environ, env, clear=True):
                self.assertEqual(d.main(["--dockerfile", dockerfile, "--output", out]), 1)
            self.assertFalse(os.path.exists(out))


if __name__ == "__main__":
    unittest.main()
