"""Docker Hardened Images' VEX statements, restated so Grype applies them to Fanfold's image.

    DHI_USERNAME=... DHI_TOKEN=... python3 -I scripts/dhi_vex.py --dockerfile Dockerfile \\
        --output fanfold.openvex.json
    grype fanfold:ci --vex fanfold.openvex.json

Docker publishes, next to every DHI image (as OCI referrers, in-toto attestations), an OpenVEX
document: for each CVE a scanner would report in a package of the image, whether Docker found
the image not affected, and why. Grype ignores those statements for our image as published:
they name DHI's image as the product, and DHI's *source* packages (`pkg:deb/debian/openssl`),
where Grype reports *binary* packages (`pkg:deb/debian/libssl3t64?upstream=openssl`).

So this script fetches, for the python-base image pinned in the Dockerfile, DHI's OpenVEX and
DHI's SPDX SBOM (both for the platform Fanfold ships), maps each source package to the binary
packages built from it (the SBOM's purls carry `upstream=`), and writes the same statements with
those binary packages as the products. Grype matches a statement whose product is the
package's own purl in image and SBOM scans alike (grype/vex/openvex, findMatchingStatement).

What is restated, and what is not:
  * only `not_affected` and `fixed`, the statuses that hide a finding; an `affected` one would
    only repeat what the scanner already reports;
  * exactly the package versions DHI's SBOM lists: a package Fanfold's image gets elsewhere, or
    in another version, matches no statement;
  * DHI's justifications as they are. Most say the vulnerable code "cannot be controlled by an
    adversary" in the image: a claim about how the image is used, which holds for Fanfold, a
    Python application on DHI's Python image, the use it is made for.

Trust: the documents come from dhi.io over TLS, by digest; each blob is checked against its
digest and each statement's subject against the image's. (DHI's Sigstore signatures are not
verified here.) Standard library only, so any runner's Python can run it.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import sys
import urllib.parse
import urllib.request
from dataclasses import dataclass

REGISTRY = "dhi.io"
OPENVEX = "https://openvex.dev/ns/v0.2.0"
SPDX = "https://spdx.dev/Document"
HIDING = ("not_affected", "fixed")
BASE_STAGE = "python-base"

INDEX_TYPES = (
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
)
MANIFEST_TYPES = (
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
)


class Failure(Exception):
    """Something this script cannot work around; the message says what."""


# ------------------------------------------------------------------------------- purls


@dataclass(frozen=True)
class Purl:
    type: str
    name: str
    version: str  # percent-decoded
    upstream: str | None  # the source package (deb), from the `upstream` qualifier
    upstream_version: str | None
    bare: str  # the purl without qualifiers or subpath, as written

    @property
    def source(self) -> tuple[str, str, str]:
        """(type, source package, source version): what DHI's VEX names."""
        return (self.type, self.upstream or self.name, self.upstream_version or self.version)


def parse_purl(text: str) -> Purl | None:
    """The parts of a purl this script needs; None for anything that is not one."""
    if not text.startswith("pkg:"):
        return None
    head, _, query = text.partition("#")[0].partition("?")
    match = re.fullmatch(r"pkg:([a-z0-9.+-]+)/(?:.+/)?([^/@]+)@([^/@]+)", head)
    if not match:
        return None
    kind, name, version = match.groups()
    qualifiers = dict(urllib.parse.parse_qsl(query))
    upstream, upstream_version = None, None
    if qualifiers.get("upstream"):
        upstream, _, upstream_version = qualifiers["upstream"].partition("@")
        upstream_version = urllib.parse.unquote(upstream_version) or None
    return Purl(
        kind,
        urllib.parse.unquote(name),
        urllib.parse.unquote(version),
        upstream,
        upstream_version,
        head,
    )


# --------------------------------------------------------------------------- restating


def binaries_by_source(spdx: dict) -> dict[tuple[str, str, str], set[str]]:
    """(type, source package, version) -> the purls of the packages built from it."""
    found: dict[tuple[str, str, str], set[str]] = {}
    for package in spdx.get("packages") or []:
        for ref in package.get("externalRefs") or []:
            if ref.get("referenceType") != "purl":
                continue
            purl = parse_purl(str(ref.get("referenceLocator") or ""))
            if purl is not None:
                found.setdefault(purl.source, set()).add(purl.bare)
    return found


def restate(vex: dict, spdx: dict, base: str) -> tuple[dict, int]:
    """DHI's statements with binary packages as products; and how many had none in the SBOM."""
    binaries = binaries_by_source(spdx)
    statements, unmatched = [], 0
    for statement in vex.get("statements") or []:
        if statement.get("status") not in HIDING:
            continue
        products: set[str] = set()
        for product in statement.get("products") or []:
            purl = parse_purl(str(product.get("@id") or ""))
            if purl is not None:
                products |= binaries.get(purl.source, set())
        if not products:
            unmatched += 1
            continue
        kept = {k: statement[k] for k in ("vulnerability", "status", "justification",
                                          "impact_statement", "action_statement", "timestamp")
                if k in statement}
        kept["products"] = [{"@id": purl} for purl in sorted(products)]
        statements.append(kept)
    statements.sort(key=lambda s: (str(s["vulnerability"].get("name")), s["products"][0]["@id"]))
    digest = base.rpartition("@")[2]
    document = {
        "@context": vex.get("@context", "https://openvex.dev/ns/v0.2.0"),
        "@id": "https://github.com/arlllk/Fanfold/vex/%s" % digest.replace(":", "-"),
        "author": "Fanfold CI, restating Docker Hardened Images' %s" % vex.get("@id", "VEX"),
        "timestamp": vex.get("timestamp", "1970-01-01T00:00:00Z"),
        "version": 1,
        "statements": statements,
    }
    return document, unmatched


def empty_document(why: str) -> dict:
    return {
        "@context": "https://openvex.dev/ns/v0.2.0",
        "@id": "https://github.com/arlllk/Fanfold/vex/none",
        "author": "Fanfold CI (%s)" % why,
        "timestamp": "1970-01-01T00:00:00Z",
        "version": 1,
        "statements": [],
    }


def base_image(dockerfile: str) -> str | None:
    """The pinned dhi.io image of the python-base stage; None if that stage is not from dhi.io."""
    match = re.search(r"(?im)^FROM\s+(\S+)\s+AS\s+%s\s*$" % re.escape(BASE_STAGE), dockerfile)
    if match is None:
        raise Failure("the Dockerfile has no `FROM ... AS %s` stage" % BASE_STAGE)
    ref = match.group(1)
    if not ref.startswith(REGISTRY + "/"):
        return None
    if not re.search(r"@sha256:[0-9a-f]{64}$", ref):
        raise Failure("%s is not pinned by digest" % ref)
    return ref


# ---------------------------------------------------------------------------- registry


class Registry:
    """Just enough of the OCI distribution API to read one image's attestations."""

    def __init__(self, repository: str, username: str, token: str) -> None:
        self.repository = repository
        basic = base64.b64encode(("%s:%s" % (username, token)).encode()).decode()
        query = urllib.parse.urlencode(
            {"service": "registry.docker.io", "scope": "repository:%s:pull" % repository}
        )
        answer = self._json(f"https://{REGISTRY}/token?{query}", {"Authorization": "Basic " + basic})
        bearer = answer.get("token") or answer.get("access_token")
        if not bearer:
            raise Failure("dhi.io gave no token: check DHI_USERNAME and DHI_TOKEN")
        self.auth = {"Authorization": "Bearer " + bearer}

    @staticmethod
    def _get(url: str, headers: dict[str, str]) -> bytes:
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as r:
                return r.read()
        except OSError as exc:
            raise Failure("GET %s: %s" % (url.split("?")[0], exc)) from exc

    def _json(self, url: str, headers: dict[str, str]) -> dict:
        value = json.loads(self._get(url, headers))
        if not isinstance(value, dict):
            raise Failure("%s did not answer with a JSON object" % url)
        return value

    def manifest(self, digest: str, accept: tuple[str, ...]) -> dict:
        url = f"https://{REGISTRY}/v2/{self.repository}/manifests/{digest}"
        return self._json(url, dict(self.auth, Accept=", ".join(accept)))

    def referrers(self, digest: str) -> list[dict]:
        url = f"https://{REGISTRY}/v2/{self.repository}/referrers/{digest}"
        index = self._json(url, dict(self.auth, Accept=INDEX_TYPES[0]))
        return list(index.get("manifests") or [])

    def blob(self, digest: str) -> bytes:
        data = self._get(f"https://{REGISTRY}/v2/{self.repository}/blobs/{digest}", self.auth)
        if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
            raise Failure("blob %s does not match its digest" % digest)
        return data


def platform_digest(registry: Registry, digest: str, platform: str) -> str:
    """The digest of the image for `platform` (`digest` itself if it is not an index)."""
    index = registry.manifest(digest, INDEX_TYPES + MANIFEST_TYPES)
    if index.get("mediaType") not in INDEX_TYPES and "manifests" not in index:
        return digest
    os_name, _, arch = platform.partition("/")
    for entry in index.get("manifests") or []:
        p = entry.get("platform") or {}
        if p.get("os") == os_name and p.get("architecture") == arch:
            return str(entry["digest"])
    raise Failure("%s has no %s image" % (digest, platform))


def attestation(registry: Registry, subject: str, predicate_type: str) -> dict:
    """The predicate of the attestation of `predicate_type` about `subject`."""
    found = [m for m in registry.referrers(subject)
             if (m.get("annotations") or {}).get("in-toto.io/predicate-type") == predicate_type]
    if not found:
        raise Failure("no %s attestation for %s" % (predicate_type, subject))
    manifest = registry.manifest(str(found[0]["digest"]), MANIFEST_TYPES)
    layers = manifest.get("layers") or []
    if not layers:
        raise Failure("the %s attestation has no content" % predicate_type)
    statement = json.loads(registry.blob(str(layers[0]["digest"])))
    if "payload" in statement and "payloadType" in statement:  # a DSSE envelope
        statement = json.loads(base64.b64decode(statement["payload"]))
    subjects = {"sha256:" + str((s.get("digest") or {}).get("sha256")) for s in statement.get("subject") or []}
    if subject not in subjects:
        raise Failure("the %s attestation is about %s, not %s" % (predicate_type, sorted(subjects), subject))
    if statement.get("predicateType") != predicate_type:
        raise Failure("the attestation is a %s, not a %s" % (statement.get("predicateType"), predicate_type))
    predicate = statement.get("predicate")
    if not isinstance(predicate, dict):
        raise Failure("the %s attestation has no predicate" % predicate_type)
    return predicate


# -------------------------------------------------------------------------------- main


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dockerfile", required=True, help="the Dockerfile whose python-base to read")
    parser.add_argument("--output", required=True, help="where to write the OpenVEX document")
    parser.add_argument("--platform", default="linux/amd64", help="the platform Fanfold ships")
    args = parser.parse_args(argv)
    try:
        with open(args.dockerfile, encoding="utf-8") as handle:
            base = base_image(handle.read())
        if base is None:
            document = empty_document("the base image is not a Docker Hardened Image")
            print("::notice::the python-base image is not from dhi.io: no VEX statements to restate")
        else:
            username, token = os.environ.get("DHI_USERNAME"), os.environ.get("DHI_TOKEN")
            if not username or not token:
                raise Failure("DHI_USERNAME and DHI_TOKEN must be set to read %s" % base)
            name, _, digest = base.partition("@")
            repository = name.removeprefix(REGISTRY + "/").rpartition(":")[0] or name.removeprefix(REGISTRY + "/")
            registry = Registry(repository, username, token)
            image = platform_digest(registry, digest, args.platform)
            vex = attestation(registry, image, OPENVEX)
            spdx = attestation(registry, image, SPDX)
            document, unmatched = restate(vex, spdx, base)
            print("%s (%s): %d of %d statements restated for its binary packages; %d named a "
                  "package its SBOM does not list" % (base, args.platform, len(document["statements"]),
                                                      len(vex.get("statements") or []), unmatched))
    except Failure as exc:
        print("::error::%s" % exc, file=sys.stderr)
        return 1
    with open(args.output, "w", encoding="utf-8") as handle:
        json.dump(document, handle, indent=1, sort_keys=True)
        handle.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
