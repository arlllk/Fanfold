"""Unit tests for vendor.py (offline): python3 -I -B -m unittest discover -s scripts"""

from __future__ import annotations

import io
import tarfile
import tempfile
import unittest
from pathlib import Path

import vendor as v

HTMX = b"var htmx=1;"
SSE = b"var sse=1;"
LICENSE = "htmx.org and @alpinejs/csp"


def tarball(files: dict[str, bytes], top: str = "package") -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(f"{top}/{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


TARBALL = tarball({"dist/htmx.min.js": HTMX, "dist/ext/hx-sse.min.js": SSE, "README.md": b"x"})
TARBALL_URL = "https://registry.npmjs.org/htmx.org/-/htmx.org-4.0.2.tgz"
TOOL = b"\x7fELF tailwind"
SUMS = f"{v.sha256_hex(TOOL)}  ./tailwindcss-linux-x64\n{'0' * 64}  ./tailwindcss-linux-arm64\n"

LOCK_TEXT = """
[[npm]]
package = "htmx.org"
version = "4.0.2"
integrity = ""
files = [
    { from = "dist/htmx.min.js", to = "htmx.min.js", sha256 = "" },
    { from = "dist/ext/hx-sse.min.js", to = "hx-sse.min.js", sha256 = "" },
]

[[tool]]
name = "tailwindcss"
repository = "tailwindlabs/tailwindcss"
version = "4.3.5"
assets = [
    { platform = "linux-x64", file = "tailwindcss-linux-x64", sha256 = "" },
]
"""


class FakeNet:
    """Serves fixed answers by URL and records what was asked."""

    def __init__(self, answers: dict[str, bytes]) -> None:
        self.answers = answers
        self.asked: list[str] = []

    def __call__(self, url: str) -> bytes:
        self.asked.append(url)
        if url not in self.answers:
            raise v.VendorError(f"unexpected fetch {url}")
        return self.answers[url]


def registry(integrity: str | None = None, tarball_url: str = TARBALL_URL) -> FakeNet:
    meta = (
        '{"dist": {"tarball": "%s", "integrity": "%s"}}' % (tarball_url, integrity or v.npm_integrity(TARBALL))
    ).encode()
    return FakeNet(
        {
            "https://registry.npmjs.org/htmx.org/4.0.2": meta,
            TARBALL_URL: TARBALL,
            "https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.5/sha256sums.txt": SUMS.encode(),
            "https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.5/tailwindcss-linux-x64": TOOL,
        }
    )


class VendorTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def synced(self) -> v.Lock:
        return v.sync(v.parse_lock(LOCK_TEXT), self.dir, registry())

    def test_sync_fills_in_every_hash_and_writes_the_files(self) -> None:
        lock = self.synced()
        self.assertEqual(lock.npm[0].integrity, v.npm_integrity(TARBALL))
        self.assertEqual([f.sha256 for f in lock.npm[0].files], [v.sha256_hex(HTMX), v.sha256_hex(SSE)])
        self.assertEqual(lock.tools[0].assets[0].sha256, v.sha256_hex(TOOL))
        self.assertEqual((self.dir / "htmx.min.js").read_bytes(), HTMX)
        self.assertEqual((self.dir / "hx-sse.min.js").read_bytes(), SSE)
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["htmx.min.js", "hx-sse.min.js"])

    def test_the_rendered_lock_parses_back_to_the_same_lock(self) -> None:
        lock = self.synced()
        self.assertEqual(v.parse_lock(v.render_lock(lock)), lock)
        self.assertIn('version = "4.0.2"\n', v.render_lock(lock), "Renovate edits this line")

    def test_the_committed_lock_is_in_the_rendered_layout(self) -> None:
        text = v.LOCK.read_text(encoding="utf-8")
        self.assertEqual(v.render_lock(v.parse_lock(text)), text)

    def test_a_tarball_that_does_not_match_its_integrity_is_refused(self) -> None:
        bad = registry(integrity=v.npm_integrity(b"something else"))
        with self.assertRaisesRegex(v.VendorError, "does not match its integrity"):
            v.sync(v.parse_lock(LOCK_TEXT), self.dir, bad)
        self.assertEqual(list(self.dir.iterdir()), [], "nothing written")

    def test_a_locked_integrity_the_registry_no_longer_serves_is_an_error(self) -> None:
        lock = v.parse_lock(LOCK_TEXT)
        lock.npm[0].integrity = "sha512-" + "A" * 86 + "=="
        with self.assertRaisesRegex(v.VendorError, "integrity changed"):
            v.sync(lock, self.dir, registry())

    def test_a_tarball_off_the_registry_is_refused(self) -> None:
        net = registry(tarball_url="https://evil.example/htmx.tgz")
        with self.assertRaisesRegex(v.VendorError, "not on the registry"):
            v.sync(v.parse_lock(LOCK_TEXT), self.dir, net)

    def test_sync_removes_files_no_longer_locked(self) -> None:
        (self.dir / "old.min.js").write_text("x")
        (self.dir / "vendor.toml").write_text("kept")
        self.synced()
        self.assertFalse((self.dir / "old.min.js").exists())
        self.assertTrue((self.dir / "vendor.toml").exists())

    def test_check_passes_on_a_synced_folder(self) -> None:
        lock = self.synced()
        self.assertEqual(v.check(lock, self.dir, LICENSE), [])

    def test_check_finds_edited_missing_extra_and_unlicensed_files(self) -> None:
        lock = self.synced()
        (self.dir / "htmx.min.js").write_bytes(b"var htmx=2;")
        (self.dir / "hx-sse.min.js").unlink()
        (self.dir / "stray.js").write_text("x")
        problems = v.check(lock, self.dir, "nothing")
        self.assertEqual(len(problems), 4, problems)
        joined = "\n".join(problems)
        for expect in ["htmx.min.js: does not match", "hx-sse.min.js: missing", "stray.js", "LICENSES.txt"]:
            self.assertIn(expect, joined)

    def test_check_reports_a_lock_that_was_bumped_but_not_synced(self) -> None:
        lock = self.synced()
        lock.npm[0].integrity = ""
        lock.npm[0].files[0].sha256 = ""
        problems = v.check(lock, self.dir, LICENSE)
        self.assertEqual(len(problems), 2, problems)

    def test_the_tool_is_downloaded_once_and_hash_checked(self) -> None:
        lock = self.synced()
        net = registry()
        path = v.tool_binary(lock.tool("tailwindcss"), self.dir / "tools", net, "linux-x64")
        self.assertEqual(path.read_bytes(), TOOL)
        self.assertTrue(path.stat().st_mode & 0o100, "executable")
        v.tool_binary(lock.tool("tailwindcss"), self.dir / "tools", net, "linux-x64")
        self.assertEqual(len(net.asked), 1, "the second call uses the cached binary")

    def test_a_tool_download_that_does_not_match_is_refused(self) -> None:
        lock = self.synced()
        lock.tools[0].assets[0].sha256 = "1" * 64
        with self.assertRaisesRegex(v.VendorError, "does not match"):
            v.tool_binary(lock.tool("tailwindcss"), self.dir / "tools", registry(), "linux-x64")
        with self.assertRaisesRegex(v.VendorError, "not locked for"):
            v.tool_binary(lock.tool("tailwindcss"), self.dir / "tools", registry(), "plan9-mips")

    def test_sha256sums_lines_are_read_with_or_without_a_dot_slash(self) -> None:
        sums = v.parse_sha256sums(f"{'a' * 64}  ./x\n{'B' * 64} *y\njunk\n")
        self.assertEqual(sums, {"x": "a" * 64, "y": "b" * 64})

    def test_only_the_named_regular_file_is_read_from_a_tarball(self) -> None:
        self.assertEqual(v.read_member(TARBALL, "dist/htmx.min.js"), HTMX)
        self.assertEqual(v.read_member(tarball({"a.js": b"1"}, top="htmx"), "a.js"), b"1")
        with self.assertRaisesRegex(v.VendorError, "has no"):
            v.read_member(TARBALL, "../../etc/passwd")

    def test_hostile_lock_values_are_refused(self) -> None:
        for bad in [
            LOCK_TEXT.replace('to = "htmx.min.js"', 'to = "../app.js"'),
            LOCK_TEXT.replace('to = "htmx.min.js"', 'to = "vendor.toml"'),
            LOCK_TEXT.replace('from = "dist/htmx.min.js"', 'from = "/etc/passwd"'),
            LOCK_TEXT.replace('package = "htmx.org"', 'package = "htmx.org/../x"'),
            LOCK_TEXT.replace('version = "4.0.2"', 'version = "latest"'),
            LOCK_TEXT.replace('repository = "tailwindlabs/tailwindcss"', 'repository = "a/b/c"'),
            LOCK_TEXT.replace('to = "hx-sse.min.js"', 'to = "htmx.min.js"'),
            LOCK_TEXT + "\n[extra]\n",
            "not toml [",
        ]:
            with self.subTest(bad=bad[-60:]), self.assertRaises(v.VendorError):
                v.parse_lock(bad)

    def test_only_https_is_fetched(self) -> None:
        with self.assertRaisesRegex(v.VendorError, "not https"):
            v.fetch("http://registry.npmjs.org/htmx.org")
        with self.assertRaisesRegex(v.VendorError, "not https"):
            v.fetch("file:///etc/passwd")


if __name__ == "__main__":
    unittest.main()
