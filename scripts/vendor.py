"""The web UI's third-party files: self-hosted scripts, and the Tailwind CSS compiler.

    python3 -I scripts/vendor.py check          # offline: the vendored files are the locked ones
    python3 -I scripts/vendor.py sync           # download what the lock's versions name, re-lock
    python3 -I scripts/vendor.py css            # compile app.css with the locked Tailwind
    python3 -I scripts/vendor.py css --check    # fail if the committed app.css is stale

The lock is crates/adapters/web/assets/vendor/vendor.toml. People and Renovate change only its
`version` lines; `sync` fills in the rest: for an npm package, the tarball's integrity (as the
registry publishes it, checked against the download) and the sha256 of each file taken out of
it; for a tool, the sha256 of each platform's binary (from the release's sha256sums.txt, checked
against the download). The vendor-sync workflow runs `sync` on Renovate's branches.

`check` needs no network: every file the lock lists is in the vendor folder with the locked
hash, nothing else is there, and LICENSES.txt names every package. CI runs it, so a version
bump without its files (or a hand-edited file) cannot merge.

The Tailwind binary is not committed: `css` downloads the locked release into
target/tools/, checks its sha256, and runs it. Its output, app.css, is committed, so building
and testing the Rust crates never needs it.

Trust: a new version is trusted on first use, from the registry or GitHub over TLS; the
hashes then sit in the lock, reviewed in the pull request that changes them. Only the named
files are read out of a tarball; nothing in it is extracted or run. Standard library only.
"""

from __future__ import annotations

import argparse
import base64
import dataclasses
import hashlib
import io
import json
import platform
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.parse
import urllib.request
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / "crates" / "adapters" / "web"
VENDOR = WEB / "assets" / "vendor"
LOCK = VENDOR / "vendor.toml"
LICENSES = VENDOR / "LICENSES.txt"
CSS_INPUT = WEB / "assets" / "app.src.css"
CSS_OUTPUT = WEB / "assets" / "app.css"
TOOLS = ROOT / "target" / "tools"

NPM_REGISTRY = "https://registry.npmjs.org"
GITHUB = "https://github.com"
USER_AGENT = "fanfold-vendor"
TIMEOUT_S = 60
# Files in the vendor folder that the lock does not list.
OWN_FILES = {"vendor.toml", "LICENSES.txt"}

HEADER = """\
# The web UI's third-party files, self-hosted (no CDN). See scripts/vendor.py.
#
# Change only the `version` lines (Renovate does, see renovate.json), then run
# `just vendor` (the vendor-sync workflow does it on Renovate's branches): it downloads
# those versions and fills in every hash below. `just vendor-check` (CI) verifies the files.
"""

SAFE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SAFE_PATH = re.compile(r"^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$")
NPM_NAME = re.compile(r"^(@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$")
REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")


class VendorError(Exception):
    """Something about the lock, a download or a file is wrong; the message says what."""


# ----------------------------------------------------------------------------------- the lock


@dataclasses.dataclass
class VendoredFile:
    # Path inside the package (npm tarballs have it under `package/`).
    source: str
    # File name in the vendor folder.
    target: str
    sha256: str = ""


@dataclasses.dataclass
class NpmPackage:
    package: str
    version: str
    files: list[VendoredFile]
    integrity: str = ""


@dataclasses.dataclass
class ToolAsset:
    platform: str
    file: str
    sha256: str = ""


@dataclasses.dataclass
class Tool:
    name: str
    repository: str
    version: str
    assets: list[ToolAsset]


@dataclasses.dataclass
class Lock:
    npm: list[NpmPackage]
    tools: list[Tool]

    def tool(self, name: str) -> Tool:
        for tool in self.tools:
            if tool.name == name:
                return tool
        raise VendorError(f"the lock has no tool {name!r}")


def _str(table: dict[str, object], key: str, where: str, pattern: re.Pattern[str] | None = None) -> str:
    value = table.get(key, "")
    if not isinstance(value, str):
        raise VendorError(f"{where}: {key} must be a string")
    if pattern is not None and value and not pattern.match(value):
        raise VendorError(f"{where}: {key} {value!r} is not allowed")
    return value


def _tables(value: object, where: str) -> list[dict[str, object]]:
    if not isinstance(value, list) or not all(isinstance(v, dict) for v in value):
        raise VendorError(f"{where} must be a list of tables")
    return value


def parse_lock(text: str) -> Lock:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise VendorError(f"vendor.toml: {e}") from e
    unknown = set(data) - {"npm", "tool"}
    if unknown:
        raise VendorError(f"vendor.toml: unknown sections {sorted(unknown)}")
    npm: list[NpmPackage] = []
    for t in _tables(data.get("npm", []), "npm"):
        where = f"npm {t.get('package')!r}"
        files = [
            VendoredFile(
                source=_str(f, "from", where, SAFE_PATH),
                target=_str(f, "to", where, SAFE_NAME),
                sha256=_str(f, "sha256", where, SHA256),
            )
            for f in _tables(t.get("files", []), f"{where} files")
        ]
        package = NpmPackage(
            package=_str(t, "package", where, NPM_NAME),
            version=_str(t, "version", where, VERSION),
            integrity=_str(t, "integrity", where),
            files=files,
        )
        if not package.package or not package.version or not files:
            raise VendorError(f"{where}: package, version and files are required")
        if package.integrity and not package.integrity.startswith("sha512-"):
            raise VendorError(f"{where}: integrity must be a sha512 one")
        npm.append(package)
    tools: list[Tool] = []
    for t in _tables(data.get("tool", []), "tool"):
        where = f"tool {t.get('name')!r}"
        tool = Tool(
            name=_str(t, "name", where, SAFE_NAME),
            repository=_str(t, "repository", where, REPOSITORY),
            version=_str(t, "version", where, VERSION),
            assets=[
                ToolAsset(
                    platform=_str(a, "platform", where, SAFE_NAME),
                    file=_str(a, "file", where, SAFE_NAME),
                    sha256=_str(a, "sha256", where, SHA256),
                )
                for a in _tables(t.get("assets", []), f"{where} assets")
            ],
        )
        if not tool.name or not tool.repository or not tool.version or not tool.assets:
            raise VendorError(f"{where}: name, repository, version and assets are required")
        tools.append(tool)
    targets = [f.target for p in npm for f in p.files]
    if len(targets) != len(set(targets)):
        raise VendorError("vendor.toml: two files are vendored under the same name")
    if set(targets) & OWN_FILES:
        raise VendorError(f"vendor.toml: {sorted(OWN_FILES)} are reserved names")
    return Lock(npm=npm, tools=tools)


def render_lock(lock: Lock) -> str:
    """The lock as text. The layout is fixed, so `version` stays the line Renovate edits."""
    out = [HEADER]
    for p in lock.npm:
        out.append("\n[[npm]]\n")
        out.append(f'package = "{p.package}"\n')
        out.append(f'version = "{p.version}"\n')
        out.append(f'integrity = "{p.integrity}"\n')
        out.append("files = [\n")
        for f in p.files:
            out.append(f'    {{ from = "{f.source}", to = "{f.target}", sha256 = "{f.sha256}" }},\n')
        out.append("]\n")
    for t in lock.tools:
        out.append("\n[[tool]]\n")
        out.append(f'name = "{t.name}"\n')
        out.append(f'repository = "{t.repository}"\n')
        out.append(f'version = "{t.version}"\n')
        out.append("assets = [\n")
        for a in t.assets:
            out.append(f'    {{ platform = "{a.platform}", file = "{a.file}", sha256 = "{a.sha256}" }},\n')
        out.append("]\n")
    return "".join(out)


# ------------------------------------------------------------------------------------ hashing


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def npm_integrity(data: bytes) -> str:
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")


def parse_sha256sums(text: str) -> dict[str, str]:
    """`<hex>  <file>` lines (sha256sum's format; a `*` before the name means binary mode)."""
    sums: dict[str, str] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and SHA256.match(parts[0].lower()):
            name = parts[1].lstrip("*")
            sums[name.removeprefix("./")] = parts[0].lower()
    return sums


def read_member(tarball: bytes, path: str) -> bytes:
    """One regular file out of an npm tarball, by its path under `package/`."""
    with tarfile.open(fileobj=io.BytesIO(tarball), mode="r:gz") as tar:
        for member in tar.getmembers():
            # npm packs everything under one top folder, normally `package/`.
            _, _, inner = member.name.partition("/")
            if inner == path:
                if not member.isfile():
                    raise VendorError(f"{path} in the tarball is not a regular file")
                extracted = tar.extractfile(member)
                if extracted is None:
                    raise VendorError(f"cannot read {path} from the tarball")
                return extracted.read()
    raise VendorError(f"the tarball has no {path}")


# ---------------------------------------------------------------------------------- the network

Fetch = Callable[[str], bytes]


def fetch(url: str) -> bytes:
    scheme = urllib.parse.urlsplit(url).scheme
    if scheme != "https":
        raise VendorError(f"refusing to fetch {url}: not https")
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_S) as response:
            return response.read()
    except OSError as e:
        raise VendorError(f"cannot fetch {url}: {e}") from e


def npm_metadata(package: NpmPackage, get: Fetch) -> tuple[str, str]:
    """(tarball URL, integrity) the registry publishes for the package's version."""
    name = urllib.parse.quote(package.package, safe="@")
    url = f"{NPM_REGISTRY}/{name}/{urllib.parse.quote(package.version)}"
    try:
        meta = json.loads(get(url))
        dist = meta["dist"]
        tarball, integrity = dist["tarball"], dist["integrity"]
    except (ValueError, KeyError, TypeError) as e:
        raise VendorError(f"{package.package} {package.version}: unexpected registry answer") from e
    if not isinstance(tarball, str) or not tarball.startswith(f"{NPM_REGISTRY}/"):
        raise VendorError(f"{package.package}: tarball {tarball!r} is not on the registry")
    if not isinstance(integrity, str) or not integrity.startswith("sha512-"):
        raise VendorError(f"{package.package}: the registry gives no sha512 integrity")
    return tarball, integrity


# --------------------------------------------------------------------------------- operations


def sync(lock: Lock, vendor: Path, get: Fetch) -> Lock:
    """Download every file the lock's versions name into `vendor`, and the hashes into the lock.

    A version whose integrity is already locked must still match it: a registry that
    started serving something else for the same version is an error, not an update.
    """
    wanted: set[str] = set()
    for package in lock.npm:
        tarball_url, integrity = npm_metadata(package, get)
        if package.integrity and package.integrity != integrity:
            raise VendorError(
                f"{package.package} {package.version}: the registry's integrity changed "
                f"({package.integrity} locked, {integrity} published)"
            )
        tarball = get(tarball_url)
        if npm_integrity(tarball) != integrity:
            raise VendorError(f"{package.package} {package.version}: the tarball does not match its integrity")
        package.integrity = integrity
        for f in package.files:
            data = read_member(tarball, f.source)
            f.sha256 = sha256_hex(data)
            (vendor / f.target).write_bytes(data)
            wanted.add(f.target)
    for tool in lock.tools:
        base = f"{GITHUB}/{tool.repository}/releases/download/v{tool.version}"
        sums = parse_sha256sums(get(f"{base}/sha256sums.txt").decode("utf-8", "replace"))
        for asset in tool.assets:
            published = sums.get(asset.file)
            if published is None:
                raise VendorError(f"{tool.name} v{tool.version}: sha256sums.txt has no {asset.file}")
            if asset.sha256 and asset.sha256 != published:
                raise VendorError(f"{tool.name} v{tool.version}: the published sha256 of {asset.file} changed")
            asset.sha256 = published
    for stale in sorted(p.name for p in vendor.iterdir() if p.name not in wanted | OWN_FILES):
        (vendor / stale).unlink()
    return lock


def check(lock: Lock, vendor: Path, licenses: str) -> list[str]:
    """What is wrong with the vendor folder (nothing: an empty list). No network."""
    problems: list[str] = []
    listed: set[str] = set()
    for package in lock.npm:
        if not package.integrity:
            problems.append(f"{package.package}: no integrity locked; run `just vendor`")
        if package.package not in licenses:
            problems.append(f"{package.package}: not named in LICENSES.txt")
        for f in package.files:
            listed.add(f.target)
            path = vendor / f.target
            if not f.sha256:
                problems.append(f"{f.target}: no sha256 locked; run `just vendor`")
            elif not path.is_file():
                problems.append(f"{f.target}: missing; run `just vendor`")
            elif sha256_hex(path.read_bytes()) != f.sha256:
                problems.append(f"{f.target}: does not match the lock (edited by hand, or not synced)")
    for tool in lock.tools:
        for asset in tool.assets:
            if not asset.sha256:
                problems.append(f"{tool.name} {asset.platform}: no sha256 locked; run `just vendor`")
    for extra in sorted(p.name for p in vendor.iterdir() if p.name not in listed | OWN_FILES):
        problems.append(f"{extra}: in the vendor folder but not in the lock")
    return problems


def host_platform() -> str:
    system = {"Linux": "linux", "Darwin": "macos"}.get(platform.system(), platform.system().lower())
    machine = {"x86_64": "x64", "amd64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(
        platform.machine().lower(), platform.machine().lower()
    )
    return f"{system}-{machine}"


def tool_binary(tool: Tool, tools_dir: Path, get: Fetch, plat: str | None = None) -> Path:
    """The locked tool for this machine, downloaded once into `tools_dir` and hash-checked."""
    plat = plat or host_platform()
    asset = next((a for a in tool.assets if a.platform == plat), None)
    if asset is None:
        raise VendorError(f"{tool.name} is not locked for {plat}; add it to vendor.toml")
    if not asset.sha256:
        raise VendorError(f"{tool.name} {plat}: no sha256 locked; run `just vendor`")
    path = tools_dir / f"{tool.name}-{tool.version}-{plat}"
    if path.is_file() and sha256_hex(path.read_bytes()) == asset.sha256:
        return path
    url = f"{GITHUB}/{tool.repository}/releases/download/v{tool.version}/{asset.file}"
    data = get(url)
    if sha256_hex(data) != asset.sha256:
        raise VendorError(f"{tool.name} {plat}: the download does not match the locked sha256")
    tools_dir.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(".partial")
    partial.write_bytes(data)
    partial.chmod(0o755)
    partial.replace(path)
    return path


def build_css(tailwind: Path, output: Path) -> None:
    result = subprocess.run(
        [str(tailwind), "--input", str(CSS_INPUT), "--output", str(output), "--minify"],
        cwd=CSS_INPUT.parent,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise VendorError(f"tailwindcss failed:\n{result.stderr}")


# ------------------------------------------------------------------------------------ the CLI


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check", help="verify the vendored files against the lock (offline)")
    sub.add_parser("sync", help="download the locked versions and fill in their hashes")
    css = sub.add_parser("css", help="compile app.css with the locked Tailwind")
    css.add_argument("--check", action="store_true", help="fail if the committed app.css is stale")
    args = parser.parse_args(argv)

    try:
        lock = parse_lock(LOCK.read_text(encoding="utf-8"))
        if args.command == "check":
            licenses = LICENSES.read_text(encoding="utf-8") if LICENSES.is_file() else ""
            problems = check(lock, VENDOR, licenses)
            for problem in problems:
                print(f"vendor: {problem}", file=sys.stderr)
            if not problems:
                print("vendor: every file matches the lock")
            return 1 if problems else 0
        if args.command == "sync":
            LOCK.write_text(render_lock(sync(lock, VENDOR, fetch)), encoding="utf-8")
            print("vendor: synced; review the changes to vendor.toml")
            return 0
        tailwind = tool_binary(lock.tool("tailwindcss"), TOOLS, fetch)
        if not args.check:
            build_css(tailwind, CSS_OUTPUT)
            return 0
        with tempfile.TemporaryDirectory() as tmp:
            fresh = Path(tmp) / "app.css"
            build_css(tailwind, fresh)
            if not CSS_OUTPUT.is_file() or fresh.read_bytes() != CSS_OUTPUT.read_bytes():
                print("vendor: app.css is stale; run `just css` and commit it", file=sys.stderr)
                return 1
        print("vendor: app.css is up to date")
        return 0
    except (VendorError, OSError) as e:
        print(f"vendor: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

