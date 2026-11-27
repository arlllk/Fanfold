"""Tests for lock_cooldown.py (python3 -I -B -m unittest discover -s scripts). Offline: no
cargo or uv is run; the commands are recorded and the lock files are temporary."""

from __future__ import annotations

import datetime
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import lock_cooldown as lc  # noqa: E402


def lock(*packages: tuple[str, str]) -> str:
    return "version = 4\n\n" + "".join(f'[[package]]\nname = "{n}"\nversion = "{v}"\n\n' for n, v in packages)


class Versions(unittest.TestCase):
    def test_orders_semver_and_pep440(self) -> None:
        ordered = ["0.9.9", "1.0.0-alpha.1", "1.0.0-rc.1", "1.0", "1.0.1", "1.0.1.post1", "1.2.3+spec-1.1.0", "1.10.0"]
        keys = [lc.version_key(v) for v in ordered]
        self.assertEqual(keys, sorted(keys))  # type: ignore[type-var]
        self.assertEqual(lc.version_key("1.0"), lc.version_key("1.0.0"))
        self.assertLess(lc.version_key("3.0.0rc1"), lc.version_key("3.0.0"))  # type: ignore[operator]
        self.assertLess(lc.version_key("3.0.0.dev0"), lc.version_key("3.0.0"))  # type: ignore[operator]

    def test_unparsable_is_none(self) -> None:
        self.assertIsNone(lc.version_key("latest"))

    def test_semver_bucket(self) -> None:
        self.assertEqual(lc.semver_bucket("1.4.2"), (1,))
        self.assertEqual(lc.semver_bucket("0.8.13"), (0, 8))
        self.assertEqual(lc.semver_bucket("0.0.3"), (0, 0, 3))
        self.assertEqual(lc.semver_bucket("2.0.0-rc.1"), (2,))


class Downgrades(unittest.TestCase):
    def test_finds_what_went_back(self) -> None:
        old = {"jiff": {"1.0.3"}, "toml": {"1.2.3+spec-1.1.0"}, "same": {"2.0.0"}, "up": {"1.0.0"}}
        new = {"jiff": {"1.0.2"}, "toml": {"1.2.2+spec-1.1.0"}, "same": {"2.0.0"}, "up": {"1.1.0"}}
        self.assertEqual(
            lc.downgrades(old, new, cargo=True),
            [("jiff", "1.0.2", "1.0.3"), ("toml", "1.2.2+spec-1.1.0", "1.2.3+spec-1.1.0")],
        )

    def test_cargo_compares_each_compatible_range(self) -> None:
        # Two majors of one crate: the 0.8 copy went back, the 1.x copy went forward.
        old = {"rand": {"0.8.5", "1.0.1"}}
        new = {"rand": {"0.8.4", "1.0.2"}}
        self.assertEqual(lc.downgrades(old, new, cargo=True), [("rand", "0.8.4", "0.8.5")])

    def test_a_removed_range_is_not_a_downgrade(self) -> None:
        self.assertEqual(lc.downgrades({"rand": {"0.8.5", "1.0.1"}}, {"rand": {"1.0.1"}}, cargo=True), [])

    def test_uv_compares_per_package(self) -> None:
        self.assertEqual(
            lc.downgrades({"charset-normalizer": {"3.5.3"}}, {"charset-normalizer": {"3.5.2"}}, cargo=False),
            [("charset-normalizer", "3.5.2", "3.5.3")],
        )


class Changes(unittest.TestCase):
    def test_lists_changed_added_and_removed(self) -> None:
        old = {"a": {"1.0.0"}, "gone": {"0.1.0"}, "same": {"1.0.0"}}
        new = {"a": {"1.0.1"}, "added": {"2.0.0"}, "same": {"1.0.0"}}
        self.assertEqual(
            lc.changes(old, new),
            ["`a` 1.0.0 → 1.0.1", "`added` (new) → 2.0.0", "`gone` 0.1.0 → (removed)"],
        )


class StripExcludeNewer(unittest.TestCase):
    HEAD = 'version = 1\nrevision = 3\nrequires-python = ">=3.13"\n\n'
    BODY = '[[package]]\nname = "a"\nversion = "1.0.0"\n'

    def test_removes_the_options_uv_added(self) -> None:
        text = self.HEAD + '[options]\nexclude-newer = "2026-11-22T06:00:00Z"\n\n' + self.BODY
        self.assertEqual(lc.strip_exclude_newer(text, "2026-11-22T06:00:00Z"), self.HEAD + self.BODY)

    def test_fails_on_anything_unexpected(self) -> None:
        for text in (
            self.HEAD + self.BODY,  # nothing recorded
            self.HEAD + '[options]\nexclude-newer = "2000-01-01T00:00:00Z"\n\n' + self.BODY,  # another cut-off
            self.HEAD + '[options]\nexclude-newer = "2026-11-22T06:00:00Z"\nother = 1\n\n' + self.BODY,
        ):
            with self.assertRaises(lc.CooldownError):
                lc.strip_exclude_newer(text, "2026-11-22T06:00:00Z")


class Restore(unittest.TestCase):
    def test_restores_until_nothing_went_back(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "Cargo.lock"
            path.write_text(lock(("jiff", "1.0.2"), ("jiff-core", "1.0.1"), ("zip", "9.8.1")))
            old = {"jiff": {"1.0.3"}, "jiff-core": {"1.0.2"}, "zip": {"9.8.2"}}
            ran: list[list[str]] = []

            def run(argv: Sequence[str]) -> None:
                # Like cargo: restoring jiff brings jiff-core along; zip is restored on its own.
                ran.append(list(argv))
                current = dict(lc.read_lock(path))
                if argv[3] == "jiff@1.0.2":
                    current |= {"jiff": {"1.0.3"}, "jiff-core": {"1.0.2"}}
                if argv[3] == "zip@9.8.1":
                    current |= {"zip": {"9.8.2"}}
                path.write_text(lock(*((n, v) for n, vs in current.items() for v in vs)))

            lc.restore(path, old, True, lambda n, new, was: ["cargo", "update", "-p", f"{n}@{new}", "--precise", was], run)
            self.assertEqual(lc.downgrades(old, lc.read_lock(path), True), [])
            self.assertIn(["cargo", "update", "-p", "jiff@1.0.2", "--precise", "1.0.3"], ran)
            self.assertIn(["cargo", "update", "-p", "zip@9.8.1", "--precise", "9.8.2"], ran)

    def test_gives_up_instead_of_looping(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "uv.lock"
            path.write_text(lock(("a", "1.0.0")))
            with self.assertRaises(lc.CooldownError):
                lc.restore(path, {"a": {"1.0.1"}}, False, lambda n, new, was: ["noop"], lambda argv: None)


class UpdateCargo(unittest.TestCase):
    def test_applies_only_old_enough_upgrades_to_the_original_lock(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            path = root / "Cargo.lock"
            # jiff 1.0.3 is locked and newer than the cut-off; hyper and zip have old-enough
            # upgrades, but zip 9.9.0 needs crc 2.1.0, which is newer than the cut-off.
            path.write_text(lock(("jiff", "1.0.3"), ("hyper", "1.12.0"), ("zip", "9.8.1"), ("crc", "2.0.0")))
            ran: list[list[str]] = []

            def run(argv: Sequence[str]) -> None:
                ran.append(list(argv))
                if argv[0] == "env":  # every resolution runs under the cool-down
                    self.assertEqual(argv[1:3], ["CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE=3 days", "cargo"])
                    argv = argv[2:]
                if argv[1] == "generate-lockfile":  # the cut-off resolution
                    path.write_text(lock(("jiff", "1.0.2"), ("hyper", "1.13.0"), ("zip", "9.9.0"), ("crc", "2.0.0")))
                elif argv[1] == "update":
                    current = lc.read_lock(path)
                    name, want = argv[3].split("@")[0], argv[5]
                    current[name] = {want}
                    if name == "zip":
                        current["crc"] = {"2.1.0"}
                    path.write_text(lock(*((n, v) for n, vs in current.items() for v in vs)))

            original_root, lc.ROOT = lc.ROOT, root
            try:
                lines, skipped = lc.update_cargo(3, run)
            finally:
                lc.ROOT = original_root
            self.assertEqual(lc.read_lock(path), {"jiff": {"1.0.3"}, "hyper": {"1.13.0"}, "zip": {"9.8.1"}, "crc": {"2.0.0"}})
            self.assertEqual(lines, ["`hyper` 1.12.0 → 1.13.0"])
            self.assertEqual(len(skipped), 1)
            self.assertIn("crc 2.1.0", skipped[0])
            self.assertEqual(ran[0], lc.cargo_with_age(3, "generate-lockfile"))
            self.assertIn(lc.cargo_with_age(3, "update", "-p", "hyper@1.12.0", "--precise", "1.13.0"), ran)
            self.assertEqual(ran[-1][:3], ["cargo", "tree", "--locked"])


class Cutoff(unittest.TestCase):
    def test_format_cargo_and_uv_accept(self) -> None:
        now = datetime.datetime(2026, 11, 25, 6, 5, 4, 123, tzinfo=datetime.UTC)
        self.assertEqual(lc.cutoff_for(3, now), "2026-11-22T06:05:04Z")


if __name__ == "__main__":
    unittest.main()
