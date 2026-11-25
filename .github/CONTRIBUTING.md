# Contributing

Changes reach `main` only through pull requests, and every pull request runs CI
(`.github/workflows/ci.yml`): formatting, clippy, the tests, the PostgreSQL and FanFicFare
integration tests, and a hardened smoke test of the container image. The `ci-ok` check has to
be green before anything merges.

## Branches and pull requests

Push a branch and a pull request into `main` is opened for it (`pr.yml`), titled after the
branch's first commit. You can also open one yourself; nothing is duplicated.

Those PRs are opened, and merged, by the repository's GitHub App (secrets `RELEASE_APP_ID` and
`RELEASE_APP_PRIVATE_KEY`), so CI and the release start by themselves. A PR or merge by the
built-in Actions token would trigger nothing.

The maintainer's pull requests merge themselves once CI passes (`automerge.yml`). To stop that,
make the PR a draft or add the `no-automerge` label. Pull requests from forks are merged by
hand.

## Dependencies

Nothing new is taken until it is **3 days old**. Most malicious releases (a hijacked
maintainer account, a moved tag) are found and yanked within hours to a few days, and CI cannot
tell one from a real release: it passes either way.

- **Renovate** (`renovate.json`) proposes updates once they are 3 days old. Patch updates of
  dependencies at 1.0 or later, and digest updates of base images, merge themselves once CI
  passes. Everything else waits for review: minor and major updates, anything on 0.x, every
  GitHub Action (they run next to the GitHub App's key and the signing identity, and a digest
  update is what a hijacked tag looks like), BuildKit and the Dockerfile frontend.
- **Lock files** (`lock-maintenance.yml`, Mondays) move to the newest releases that are 3 days
  old, through `scripts/lock_cooldown.py`: `cargo generate-lockfile --publish-time` (unstable,
  so on a nightly cargo, for resolving only) and `uv lock --exclude-newer`. Nothing locked ever
  goes back, so a security fix newer than that stays. Its PR merges itself once CI passes. By
  hand: `RUSTUP_TOOLCHAIN=nightly python3 -I scripts/lock_cooldown.py` (it needs a nightly
  toolchain and uv).
- **Security fixes** skip the wait: Renovate opens them at once (GitHub's alerts and
  [OSV](https://osv.dev)), labelled `security` and `release:now`. They are never merged
  without a person; merging one releases at once.

## Versions and releases

Releases are made by `release.yml`: a commit of `main` is tagged `vX.Y.Z`, a GitHub release is
created with generated notes, and the image is published to `ghcr.io/arlllk/fanfold` as `X.Y.Z`,
`X.Y`, `X` (from 1.0 on) and `latest`, for linux/amd64.

Merges are not released one by one. **Every Tuesday** `main` is released if anything the image
is built from changed since the last release; everything merged that week goes out together,
after the Monday lock file update and with every dependency through its cool-down.

Nothing is released if only CI, docs, Renovate's config, tests, test data, examples, fuzzing
or the dev tooling changed: that would be the same image under a new version. Those changes go
out with the next release.

### Urgent releases (a CVE, a bad bug)

- **A security fix from Renovate**: review and merge its PR. It carries `release:now`, so the
  merge is released right away.
- **Any other PR**: add the `release:now` label before merging it, and its merge is
  released right away.
- **Already on `main`** (merged earlier, waiting for Tuesday), such as a base image digest
  update that brings a CVE fix (Renovate merges those by itself): release it now with

  ```sh
  gh workflow run release.yml
  ```

  `-f sha=<commit>` releases an older commit of `main` instead of its head, and `-f force=true`
  releases even if no image input changed. Every input is pinned, so a forced release of the
  same commit is the same image under a new version; a fix always needs a change on `main`
  first (if Renovate has not proposed it yet, bump the version or digest by hand in a PR).

The daily scan (`scan.yml`) re-checks the latest release against new CVEs and lists what it
finds in the Security tab: that is where to look for "is the release affected?".

Choose the bump with a label on the pull request:

| label           | bump                 |
|-----------------|----------------------|
| `release:major` | 1.4.2 → 2.0.0        |
| `release:minor` | 1.4.2 → 1.5.0        |
| `release:patch` | 1.4.2 → 1.4.3 (default, also with no label) |

The biggest label of all the pull requests merged since the last release wins, so a
`release:minor` PR that was not released on its own still makes the next release a minor one.
The version exists only as the git tag (and inside the image, passed to the build as
`FANFOLD_VERSION`): nothing is committed back to `main`, so a release never puts other open pull
requests behind or in conflict. `Cargo.toml`'s version is only the starting point for the very
first release.

## Reproducible images

The same commit always builds the same image, on any machine. This was verified for v0.1.2: a
local rebuild gave exactly the layers CI published. To check a release, build its tag with the
BuildKit CI uses (pinned in `image.yml`; the files are identical with any BuildKit, but the
layer compression is BuildKit's own):

```sh
git checkout v1.4.2
docker buildx create --name repro --driver docker-container \
  --driver-opt image=moby/buildkit:v0.34.2   # the version pinned in image.yml
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) docker buildx build --builder repro \
  --build-arg FANFOLD_VERSION=1.4.2 --output type=oci,dest=fanfold.tar,rewrite-timestamp=true .
docker buildx rm repro
```

Then compare the layer digests in `fanfold.tar` with the published image's
(`docker buildx imagetools inspect --raw ghcr.io/arlllk/fanfold:1.4.2`). The image digest
itself also covers the labels CI adds, so compare the layers.

What makes it work: base images pinned by digest, crates by `Cargo.lock`, and Python packages by
hash in `python/uv.lock` (wheels only, nothing compiled). Nothing comes from apt, and every
timestamp is the commit's. Keep it that way: an unpinned download or a `date` in the Dockerfile
breaks it.

## Python

FanFicFare, the only Python dependency, is declared in `python/pyproject.toml` and locked in
`python/uv.lock`. The image, CI and `just venv` all install exactly that lock with
`uv sync --locked`. After changing `pyproject.toml`, run `uv lock --project python`.

The image runs Python 3.15 from its base, a Docker Hardened Image (`dhi.io/python`, Debian 13),
and CI's integration tests, `just venv` and Pyrefly use the same minor version. Building the
image needs `docker login dhi.io` (a free Docker account); CI logs in with the repository
secrets `DHI_USERNAME` and `DHI_TOKEN`, so pull requests from forks cannot build the image.

## Security: SBOMs, attestations, vulnerability scans

The image has no shell and no package manager. Everything in it is visible to
scanners: the Debian packages (CPython included), the Python venv, and the Rust crates (built with
`cargo auditable`, which embeds the dependency list in the binary).

- **Docker's VEX statements** for the hardened base image are applied to every scan
  (`scripts/dhi_vex.py`): Docker publishes, for each CVE a scanner finds in the base image's
  packages, whether the image is affected; the ones it is not are not reported. Each release
  carries the document it was scanned with (`fanfold-X.Y.Z.openvex.json`).
- **Every build** (PRs included) is scanned with Grype. CI fails on a critical vulnerability that
  already has a fix; everything else is listed in the log.
- **Every release** has SBOMs, SPDX and CycloneDX, attached to its GitHub release, for your own
  tools (Grype, Trivy, Dependency-Track and so on).
- **Signed attestations** for the image digest, made with GitHub's signing identity for this
  workflow: build provenance (which commit and workflow built it) and the SPDX SBOM. They
  are stored in the registry next to the image, not inside it, so the digest stays
  reproducible. Check them with:

  ```sh
  gh attestation verify oci://ghcr.io/arlllk/fanfold:1.4.2 -R arlllk/Fanfold
  ```

- **Findings** go to the repository's Security tab (code scanning). `scan.yml` re-scans the
  latest release's SBOM daily, so CVEs published after a release show up too.
