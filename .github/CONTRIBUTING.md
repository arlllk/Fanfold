# Contributing

Changes reach `main` only through pull requests, and every pull request runs CI
(`.github/workflows/ci.yml`): formatting, clippy, the tests, the PostgreSQL and FanFicFare
integration tests, and a hardened smoke test of the container image. The `ci-ok` check has to
be green before anything merges.

## Branches and pull requests

Push a branch and a pull request into `main` is opened for it (`pr.yml`), titled after the
branch's first commit. You can also open one yourself; nothing is duplicated.

The maintainer's pull requests merge themselves once CI passes (`automerge.yml`). To stop that,
make the PR a draft or add the `no-automerge` label. Pull requests from forks and from Renovate
are merged by hand.

## Versions and releases

Every merge into `main` is a release (`release.yml`): the merge commit is tagged `vX.Y.Z`, a
GitHub release is created with generated notes, and the image is published to
`ghcr.io/arlllk/fanfold` as `X.Y.Z`, `X.Y`, `X` (from 1.0 on) and `latest`, for linux/amd64.

Choose the bump with a label on the pull request:

| label           | bump                 |
|-----------------|----------------------|
| `release:major` | 1.4.2 → 2.0.0        |
| `release:minor` | 1.4.2 → 1.5.0        |
| `release:patch` | 1.4.2 → 1.4.3 (default, also with no label) |

With more than one label, the biggest bump wins. The version exists only as the git tag (and
inside the image, passed to the build as `FANFOLD_VERSION`): nothing is committed back to `main`,
so a release never puts other open pull requests behind or in conflict. `Cargo.toml`'s version
is only the starting point for the very first release.
