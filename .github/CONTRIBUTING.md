# Contributing

Changes reach `main` only through pull requests, and every pull request runs CI
(`.github/workflows/ci.yml`): formatting, clippy, the tests, the PostgreSQL and FanFicFare
integration tests, and a hardened smoke test of the container image. The `ci-ok` check has to
be green before anything merges.

## Branches and pull requests

Push a branch and a pull request into `main` is opened for it (`pr.yml`), titled after the
branch's first commit. You can also open one yourself; nothing is duplicated.

The maintainer's pull requests merge themselves once CI passes (`automerge.yml`). To stop that,
make the PR a draft or add the `no-automerge` label. Pull requests from forks are merged by
hand. Renovate merges its own lock file maintenance, digest and pin updates, and patch updates
of dependencies at 1.0 or later, once CI passes; everything else from Renovate waits for review
(`renovate.json`).

## Versions and releases

A merge into `main` that changes what the image is built from is a release (`release.yml`): the
merge commit is tagged `vX.Y.Z`, a GitHub release is created with generated notes, and the
image is published to `ghcr.io/arlllk/fanfold` as `X.Y.Z`, `X.Y`, `X` (from 1.0 on) and
`latest`, for linux/amd64.

A merge that touches only CI, docs, Renovate's config, tests, test data, examples, fuzzing or
the dev tooling is not released, since it would publish the same image again under a new
version. It goes out with the next release. (Run the release workflow by hand with `force`
to release anyway.)

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

The same commit always builds the same image, with the same digest. To check a release, build
its tag:

```sh
git checkout v1.4.2
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) docker buildx build --build-arg FANFOLD_VERSION=1.4.2 \
  --output type=oci,dest=fanfold.tar,rewrite-timestamp=true .
```

The image's labels (added by CI) are left out of this build, so compare the image's layers with
the published ones (for example `docker buildx imagetools inspect --raw`), not its digest.

What makes it work: base images pinned by digest, crates by `Cargo.lock`, and Python packages by
hash in `python/uv.lock` (wheels only, nothing compiled). Nothing comes from apt, and every
timestamp is the commit's. Keep it that way: an unpinned download or a `date` in the Dockerfile
breaks it.

## Python

FanFicFare, the only Python dependency, is declared in `python/pyproject.toml` and locked in
`python/uv.lock`. The image, CI and `just venv` all install exactly that lock with
`uv sync --locked`. After changing `pyproject.toml`, run `uv lock --project python`.
