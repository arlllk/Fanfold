"""Lock file maintenance with a cool-down: refresh every lock, but only to releases that are
at least N days old.

    python3 -I scripts/lock_cooldown.py [--days 3] [--summary FILE]

What it does, for Cargo.lock, python/uv.lock and browser/uv.lock:

1. Re-resolve everything, ignoring anything published after the cut-off (now minus N days):
   `cargo generate-lockfile --publish-time` (unstable: needs a nightly cargo, e.g.
   RUSTUP_TOOLCHAIN=nightly) and `uv lock --upgrade --exclude-newer`. Both resolve from
   scratch, so on their own they would also take back anything locked that is newer than the
   cut-off (a security fix, or whatever Renovate merged in the last N days).
2. Cargo: that resolution only says which upgrades are old enough. The original lock is put
   back and each of those upgrades applied to it with `cargo update --precise`; one that
   would pull in anything newer than the cut-off is left out (and listed).
3. uv: every package that went back is set back to exactly the version it was
   (`uv lock --upgrade-package name==version`). uv records `--exclude-newer` in uv.lock,
   which would make every later `--locked` run (CI, the image) re-resolve, so it is removed
   again, and `uv lock --check` must then pass.

Nothing is built or run: resolution only (`uv lock --no-build`, and Cargo never runs build
scripts to resolve), so the job that runs this needs no secrets. Most malicious releases are
found and yanked within hours to a few days, which is what the cool-down is for. Standard
library only.
"""

from __future__ import annotations

import argparse
import datetime
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable, Sequence
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UV_PROJECTS = ("python", "browser")
MAX_TRIES = 10

Run = Callable[[Sequence[str]], None]
# (one line per changed package, one line per upgrade that was left out)
Result = tuple[list[str], list[str]]
Versions = dict[str, set[str]]


class CooldownError(Exception):
    pass


# --------------------------------------------------------------------------- versions


def version_key(version: str) -> tuple | None:
    """A sort key for SemVer (Cargo) and PEP 440 (Python) versions; None if not one.

    Build metadata and local versions (`+...`) are ignored. A pre-release (`-rc.1`, `rc1`,
    `.dev0`) sorts before its release, a `.postN` after it."""
    match = re.fullmatch(r"v?(\d+(?:\.\d+)*)(.*)", version.split("+", 1)[0])
    if not match:
        return None
    release = [int(part) for part in match.group(1).split(".")]
    while len(release) > 1 and release[-1] == 0:
        release.pop()
    rest = match.group(2).lstrip(".-_")
    tokens = tuple((0, int(t)) if t.isdigit() else (1, t) for t in re.findall(r"\d+|[A-Za-z]+", rest))
    if not rest:
        phase: tuple = (1,)
    elif rest.startswith("post"):
        phase = (2, tokens)
    else:
        phase = (0, tokens)
    return (tuple(release), phase)


def semver_bucket(version: str) -> tuple[int, ...]:
    """The versions Cargo treats as compatible (and so locks only one of): same major, or
    same 0.minor, or the exact 0.0.patch."""
    parts = [int(p) for p in re.findall(r"\d+", version.split("-", 1)[0].split("+", 1)[0])[:3]]
    parts += [0] * (3 - len(parts))
    if parts[0]:
        return (parts[0],)
    if parts[1]:
        return (0, parts[1])
    return (0, 0, parts[2])


def read_lock(path: Path) -> Versions:
    out: Versions = {}
    for package in tomllib.loads(path.read_text("utf-8")).get("package", []):
        if "version" in package:
            out.setdefault(package["name"], set()).add(package["version"])
    return out


def downgrades(old: Versions, new: Versions, cargo: bool) -> list[tuple[str, str, str]]:
    """(name, new version, old version) for every package that went backwards.

    Cargo may lock one version per compatible range (see semver_bucket), so they are
    compared range by range; uv locks one version per package."""
    found = []
    for name in sorted(old.keys() & new.keys()):
        was, now = _newest_per_range(old[name], cargo), _newest_per_range(new[name], cargo)
        for bucket in sorted(was.keys() & now.keys()):
            if now[bucket][0] < was[bucket][0]:
                found.append((name, now[bucket][1], was[bucket][1]))
    return found


def _newest_per_range(versions: set[str], cargo: bool) -> dict[tuple, tuple[tuple, str]]:
    """The newest version in each compatible range (one range for uv): (sort key, version).
    Versions that do not parse are left out (never reported as a downgrade)."""
    newest: dict[tuple, tuple[tuple, str]] = {}
    for v in versions:
        key = version_key(v)
        if key is None:
            continue
        bucket = semver_bucket(v) if cargo else ()
        if bucket not in newest or key > newest[bucket][0]:
            newest[bucket] = (key, v)
    return newest


def changes(old: Versions, new: Versions) -> list[str]:
    """One line per package whose locked versions changed, for the pull request."""
    lines = []
    for name in sorted(old.keys() | new.keys()):
        was, now = old.get(name, set()), new.get(name, set())
        if was == now:
            continue
        sort = lambda vs: sorted(vs, key=lambda v: version_key(v) or ((), ()))  # noqa: E731
        before = ", ".join(sort(was - now)) or "(new)"
        after = ", ".join(sort(now - was)) or "(removed)"
        lines.append(f"`{name}` {before} → {after}")
    return lines


def strip_exclude_newer(text: str, cutoff: str) -> str:
    """uv.lock without the [options] table our own run added (it holds only `exclude-newer`).
    Anything else there means the lock is not what this expects: fail rather than guess."""
    pattern = re.compile(r'^\[options\]\nexclude-newer = "' + re.escape(cutoff) + r'"\n\n', re.M)
    stripped, n = pattern.subn("", text, count=1)
    if n != 1:
        raise CooldownError(f'uv.lock does not record exclude-newer = "{cutoff}" the expected way')
    return stripped


# ----------------------------------------------------------------------------- steps


def run_command(argv: Sequence[str]) -> None:
    print("+", " ".join(argv), flush=True)
    subprocess.run(argv, cwd=ROOT, check=True)


def restore(lock: Path, old: Versions, cargo: bool, precise: Callable[[str, str, str], list[str]], run: Run) -> None:
    """One package at a time, re-reading the lock in between: restoring one often restores
    others with it (jiff brings jiff-core), whose old `name@version` would then not exist."""
    attempts: dict[tuple[str, str], int] = {}
    while back := downgrades(old, read_lock(lock), cargo):
        name, new, was = back[0]
        attempts[(name, was)] = attempts.get((name, was), 0) + 1
        if attempts[(name, was)] > MAX_TRIES:
            raise CooldownError(f"{lock}: could not restore {name} {was} (still {new})")
        print(f"{lock.parent.name}/{lock.name}: {name} went back from {was} to {new}; restoring {was}")
        run(precise(name, new, was))


def not_allowed(old: Versions, allowed: Versions, now: Versions) -> list[str]:
    """`name version` for every locked version that was neither locked before nor picked by
    the cut-off resolution: one that may be newer than the cut-off."""
    return [
        f"{name} {v}"
        for name in sorted(now)
        for v in sorted(now[name] - old.get(name, set()) - allowed.get(name, set()))
    ]


def update_cargo(cutoff: str, run: Run) -> Result:
    """The cut-off resolution only says which upgrades are old enough; they are then applied
    one by one to the lock as it was (`cargo update --precise`), like a plain `cargo update`
    would. Keeping the cut-off lock itself and restoring what went back is not enough: it
    also re-points dependency edges at older versions already in the graph (errno from
    windows-sys 0.61 to 0.52, say), which no version comparison sees."""
    lock = ROOT / "Cargo.lock"
    original = lock.read_text("utf-8")
    old = read_lock(lock)
    run(["cargo", "-Zunstable-options", "generate-lockfile", "--publish-time", cutoff])
    allowed = read_lock(lock)
    lock.write_text(original, "utf-8")

    skipped: list[str] = []
    tried: set[tuple[str, str, str]] = set()
    # downgrades(allowed, now): what is still older than the cut-off resolution picked.
    while pending := [u for u in downgrades(allowed, read_lock(lock), True) if u not in tried]:
        name, have, want = pending[0]
        tried.add(pending[0])
        before = lock.read_text("utf-8")
        try:
            run(["cargo", "update", "-p", f"{name}@{have}", "--precise", want])
        except subprocess.CalledProcessError:
            lock.write_text(before, "utf-8")
            skipped.append(f"`{name}` {have} → {want}: cargo could not apply it on its own")
            continue
        if extra := not_allowed(old, allowed, read_lock(lock)):
            lock.write_text(before, "utf-8")
            skipped.append(f"`{name}` {have} → {want}: needs {', '.join(extra)}, newer than the cut-off")
    run(["cargo", "tree", "--locked", "--workspace", "--depth", "0", "--quiet"])
    return changes(old, read_lock(lock)), skipped


def update_uv(project: str, cutoff: str, run: Run) -> Result:
    lock = ROOT / project / "uv.lock"
    old = read_lock(lock)
    run(["uv", "lock", "--project", project, "--upgrade", "--no-build", "--exclude-newer", cutoff])
    lock.write_text(strip_exclude_newer(lock.read_text("utf-8"), cutoff), "utf-8")
    restore(
        lock,
        old,
        False,
        lambda name, new, was: ["uv", "lock", "--project", project, "--no-build", "--upgrade-package", f"{name}=={was}"],
        run,
    )
    run(["uv", "lock", "--project", project, "--check"])
    return changes(old, read_lock(lock)), []


def cutoff_for(days: int, now: datetime.datetime | None = None) -> str:
    now = now or datetime.datetime.now(datetime.UTC)
    return (now - datetime.timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")


def summary(cutoff: str, days: int, results: dict[str, Result]) -> str:
    out = [
        f"Every lock file refreshed to the newest releases published before {cutoff} "
        f"(at least {days} days old). Nothing locked before went back: versions newer than "
        "the cut-off that were already locked (security fixes, recent Renovate updates) stay.",
        "",
        "Made by `lock-maintenance.yml` (`scripts/lock_cooldown.py`).",
    ]
    for lock, (lines, skipped) in results.items():
        if lines:
            out += ["", f"### `{lock}`", ""] + [f"- {line}" for line in lines]
        if skipped:
            out += ["", f"Not updated in `{lock}`:", ""] + [f"- {line}" for line in skipped]
    return "\n".join(out) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--days", type=int, default=3, help="minimum release age (default 3)")
    parser.add_argument("--summary", type=Path, help="write a Markdown summary here")
    args = parser.parse_args(argv)
    if args.days < 0:
        parser.error("--days must not be negative")
    cutoff = cutoff_for(args.days)
    print(f"Cut-off: {cutoff}")
    try:
        results = {"Cargo.lock": update_cargo(cutoff, run_command)}
        for project in UV_PROJECTS:
            results[f"{project}/uv.lock"] = update_uv(project, cutoff, run_command)
    except (CooldownError, subprocess.CalledProcessError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    text = summary(cutoff, args.days, results)
    print(text)
    if args.summary:
        args.summary.write_text(text, "utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
