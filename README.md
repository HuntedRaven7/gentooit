# gentooit

**packit for Gentoo Linux.** Automate packaging upstream projects into Gentoo
ebuilds — the same way [packit](https://github.com/packit/packit) automates
packaging upstream projects into Fedora.

gentooit is a CLI tool (with a GitHub App service) that continuously moves
software between two sides of the packaging river:

- **upstream** — a developer's project on GitHub (the source of truth for
  releases)
- **downstream** — the Gentoo distribution, where ebuilds live in
  `gentoo/gentoo` or in a Gentoo overlay

```
   upstream project            downstream (Gentoo)
   ┌──────────────┐    ┌──────────────────────────────┐
   │ releases     │    │ ebuilds (overlay/gentoo repo) │
   │ tarballs     │ ─► │ Manifest + metadata.xml       │   propose-downstream
   └──────────────┘    └──────────────┬───────────────┘
                                      │ pkgcheck / emerge   build
   ┌──────────────┐                   ▼
   │ vendored     │ ◄────────────────┘                     sync-from-downstream
   │ packaging    │
   └──────────────┘
```

## Workflows

gentooit mirrors the packit workflow set for Gentoo:

| gentooit command            | Packit equivalent        | What it does |
| --------------------------- | ------------------------ | ------------ |
| `gentooit propose-downstream` | `packit propose-downstream` | Take the latest (or pinned) upstream release, derive the source archive, build the ebuild + `Manifest` + `metadata.xml`, clone the downstream overlay, create a branch, commit, and open a PR. |
| `gentooit build`            | `packit build`           | Run `pkgcheck scan` (QA) and/or `emerge` build an ebuild in the downstream check-out. Also emits a GitHub Actions workflow that builds in a `gentoo/stage3` container. |
| `gentooit sync-from-downstream` | `packit sync-from-downstream` | Copy packaging files from the downstream ebuild repository back into the upstream project via a PR. |
| `gentooit init`             | `packit init`            | Scaffold a `.gentooit.yaml` project config. |

`gentooit-service` is the server counterpart to the CLI (our analogue of
packit-service): a GitHub App webhook endpoint that triggers
`propose-downstream` on new releases and build/QA on downstream PRs.

## Installation

Requires a recent stable Rust toolchain.

```sh
cargo build --release
# binaries land in target/release/
```

## Quick start

```sh
# 1. Scaffold a project config (run inside your upstream project):
gentooit init --upstream owner/repo --downstream git@github.com:you/overlay.git

# 2. Put your credentials in ~/.config/gentooit/config.yaml:
#    github-token: ghp_...
#    git-author-name: You
#    git-author-email: you@example.com

# 3. Bump the package to the latest upstream release:
gentooit propose-downstream
```

### Example project config (`.gentooit.yaml`)

```yaml
spec_version: "1.0"
upstream:
  vcs: github
  upstream: BurntSushi/ripgrep
  package_name: ripgrep
  tag_template: "v{version}"        # e.g. "v14.1.1" -> version 14.1.1
  # version: 14.1.1                 # pin instead of "latest release"
package:
  description: Recursively search directories for a regex pattern
  homepage: https://github.com/BurntSushi/ripgrep
  license: Unlicense
  keywords: "~amd64"
  maintainer_email: me@example.com
downstream:
  - url: git@github.com:you/overlay.git      # or https://github.com/gentoo/gentoo.git
    branch: master
    category: app-misc
open_pull_request: true
```

### What propose-downstream generates

For a release `14.1.1` of `BurntSushi/ripgrep` targeting category `app-misc`, it
produces in the downstream overlay clone:

```
app-misc/ripgrep/ripgrep-14.1.1.ebuild   # EAPI=8, DESCRIPTION/HOMEPAGE/SRC_URI/...
app-misc/ripgrep/Manifest               # DIST entry: SIZE + SHA256 + SHA512
app-misc/ripgrep/metadata.xml           # maintainer, bugs-to, remote-id
```

The `Manifest` hashes are generated with the current Gentoo policy (`SHA256` +
`SHA512`), so the ebuild is immediately buildable and passes `pkgcheck`.

## Configuration reference

Files:
- **Project config** `.gentooit.yaml` at the repo root (or via `--config`),
  discovered by walking up from the current directory.
- **User config** `~/.config/gentooit/config.yaml` — credentials and identity.

Top-level keys:

| Key | Type | Meaning |
| --- | ---- | ------- |
| `upstream` | map | `vcs` (`github`/`gitlab`), `upstream` (`owner/repo`), `package_name` (PN), `tag_template`, `archive_template`, `archive_name`, `version` (pin). |
| `package` | map | Static ebuild data gentooit can't infer: `description`, `homepage`, `license`, `slot`, `keywords`, `iuse`, `depend`, `rdepend`, `bdepend`, `maintainer_email`, `maintainer_name`, `remote_id_type`. |
| `downstream` | list | Target overlay(s): `url`, `branch`, `category`, `package_dir`. `url` may be a `git@…`/`https://github.com/…` remote or a local path. |
| `files_to_sync` | list | `src`/`dest`/`delete` entries for `sync-from-downstream` (packit-style). |
| `open_pull_request` | bool | Open a PR (default true) or just commit/push locally. |

## Architecture

Rust workspace with three crates:

```
gentooit/
├── crates/
│   ├── gentooit/          # CLI binary (clap)
│   ├── gentooit-core/     # platform-agnostic library
│   │   ├── config.rs      # .gentooit.yaml + user config
│   │   ├── ebuild.rs      # ebuild model: filename/atom/version parsing, metadata extraction
│   │   ├── manifest.rs    # Manifest generation: SHA256/SHA512 hashing (Gentoo policy)
│   │   ├── metadata.rs    # metadata.xml parsing/rendering (GLEP 68, remote-id)
│   │   ├── repo.rs        # git2-based repository operations (clone/branch/commit/push)
│   │   ├── github.rs      # octocrab GitHub API client (releases, PRs, app auth)
│   │   ├── propose.rs     # propose-downstream workflow
│   │   ├── build.rs       # build/QA workflow (pkgcheck, emerge, CI workflow template)
│   │   └── sync.rs        # sync-from-downstream workflow
│   └── gentooit-service/  # GitHub App webhook service (axum)
```

Key design choices:
- **thin-`Manifest` friendly** — matches the current Gentoo policy
  (`BLAKE2B SHA512` preferred; we emit `SHA256 + SHA512`, both understood by
  `pkgcheck`/Portage).
- **EAPI 8 by default** — current stable EAPI; configurable.
- **Orchestrates, doesn't reinvent** — `build` and manifestation shell out to the
  standard tools (`pkgcheck`, `pkgdev`, `emerge`) when present, and the CI
  workflow template uses a `gentoo/stage3` container.
- **Overlay-first, gentoo/gentoo-ready** — works against any git remote
  (local path, personal overlay, or `gentoo/gentoo`); the fork-and-PR flow is
  the same as Gentoo's contribution model.

## Roadmap

- [x] Use GitHub release **assets** and `S` override detection (derive archive
  filenames that don't match `${P}`)
- [x] Ebuild "diff-bump" of an existing ebuild (preserve custom `src_*`
  functions) instead of always generating fresh
- [x] `pkgcheck scan` / `pkgdev manifest` integration in `propose-downstream`
  before opening the PR (when tools are present)
- [x] Resolve the GitHub username from the token, and support GitHub App
  authentication for PR creation
- [x] Wire `gentooit-service` webhooks to the actual workflows (queue +
  background runners)
- [ ] `sync-from-downstream` local mode that copies files into the upstream
  worktree and commits

## License

## Maintenance

- **CI** (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and `cargo audit` (RUSTSEC).
- **Renovate** (`renovate.json`) keeps Cargo dependencies up to date. Note the deliberate coupling rules: `octocrab`/`jsonwebtoken` are grouped (their majors must track each other), `hmac`/`sha2` are paired (the service pins `sha2 0.10` to match `hmac 0.12`'s digest), and `git2` major bumps require manual approval.

## License

MIT
