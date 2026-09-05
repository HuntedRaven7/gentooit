# Contributing to gentooit

gentooit automates the boring part of Gentoo packaging: turning an upstream
GitHub release into a clean ebuild, `Manifest`, and `metadata.xml`, and opening
a pull request against your overlay. This guide covers both **using gentooit in
your own overlay** and **contributing back to gentooit itself**.

## Table of contents

- [Using gentooit in your overlay](#using-gentooit-in-your-overlay)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
  - [Quick start](#quick-start)
  - [Project config (`.gentooit.yaml`)](#project-config-gentooityaml)
  - [User config (`~/.config/gentooit/config.yaml`)](#user-config-configgentooitconfigyaml)
  - [Running the workflows](#running-the-workflows)
  - [What gets generated](#what-gets-generated)
  - [Rust / cargo packages](#rust--cargo-packages)
  - [Overlay layout tips](#overlay-layout-tips)
- [Contributing to gentooit](#contributing-to-gentooit)
  - [Development setup](#development-setup)
  - [Running tests](#running-tests)
  - [Submitting a PR](#submitting-a-pr)
  - [Code style](#code-style)

---

## Using gentooit in your overlay

### Prerequisites

- **Rust** >= 1.70 (stable toolchain, `rustup` recommended)
- **Git** with push access to your overlay repository
- **GitHub** — a personal access token (PAT) with `repo` scope, or a GitHub App
  installation on the overlay repo
- Optional but recommended: **pkgcheck** and **pkgdev** from `gentoo/packages`
  for QA checks before the PR opens

### Installation

```sh
# From source (clone this repo):
git clone https://github.com/your-org/gentooit.git
cd gentooit
cargo build --release

# The binary is at target/release/gentooit
# Optionally symlink it onto your PATH:
sudo ln -sf $(pwd)/target/release/gentooit /usr/local/bin/gentooit
```

Or via `cargo install` once the crate is published:

```sh
cargo install gentooit
```

### Quick start

The minimal workflow is three steps:

```sh
# 1. Scaffold a project config inside the upstream repo (or any directory):
gentooit init \
  --upstream BurntSushi/ripgrep \
  --downstream git@github.com:you/gentoo-overlay.git \
  --category app-misc

# 2. Add your GitHub credentials:
mkdir -p ~/.config/gentooit
cat > ~/.config/gentooit/config.yaml <<'EOF'
github-token: ghp_YOUR_TOKEN_HERE
git-author-name: Your Name
git-author-email: you@example.com
EOF

# 3. Run propose-downstream from anywhere (it finds .gentooit.yaml by walking up):
cd /path/to/anywhere/above/.gentooit.yaml
gentooit propose-downstream
```

That's it. gentooit will:
1. Find the latest upstream release
2. Download the source archive
3. Generate the ebuild, `Manifest`, and `metadata.xml`
4. Clone your overlay, create a branch, commit, push, and open a PR

### Project config (`.gentooit.yaml`)

Place `.gentooit.yaml` at the root of your **upstream** project (or anywhere
convenient — gentooit walks up the directory tree to find it).

```yaml
spec_version: "1.0"
upstream:
  vcs: github                          # or gitlab (not yet implemented)
  upstream: BurntSushi/ripgrep         # owner/repo on GitHub
  package_name: ripgrep                # PN; defaults to repo name if omitted
  tag_template: "v{version}"           # e.g. tag "v14.1.1" -> version "14.1.1"
  # version: 14.1.1                    # optional: pin instead of "latest release"
  # archive_template: ...              # override source tarball URL pattern
package:
  description: Recursively search directories for a regex pattern
  homepage: https://github.com/BurntSushi/ripgrep
  license: Unlicense
  slot: "0"
  keywords: "~amd64"
  iuse: ""                             # optional
  depend: ""                           # optional
  rdepend: ""                          # optional
  bdepend: ""                          # optional
  maintainer_email: you@example.com    # optional; falls back to user config
  maintainer_name: Your Name           # optional; falls back to user config
downstream:
  - url: git@github.com:you/gentoo-overlay.git   # git remote or local path
    branch: master                               # default: master
    category: app-misc                            # required: where the ebuild lives
    # package_dir: ebuilds                        # optional: prefix inside the overlay
open_pull_request: true                             # set false to commit/push only
```

#### Key fields explained

| Field | Purpose |
|-------|---------|
| `upstream.upstream` | GitHub `owner/repo` to watch for releases |
| `upstream.package_name` | The ebuild package name (PN). Defaults to the repo name. |
| `upstream.tag_template` | How to extract a `{version}` from a git tag. Common patterns: `"v{version}"`, `"{version}"`, `"release-{version}"` |
| `upstream.version` | Pin to a specific version instead of using the latest release |
| `downstream[].url` | Your overlay repo URL. Supports SSH (`git@github.com:...`), HTTPS, or a local absolute path |
| `downstream[].category` | Gentoo category (e.g. `app-misc`, `dev-lang`, `sys-apps`) |
| `downstream[].package_dir` | Optional subdirectory inside the overlay (e.g. `ebuilds` for repos that nest packages) |
| `package.depend` / `rdepend` / `bdepend` | Pre-fill dependency variables. Leave empty and gentooit will infer sensible defaults for Rust/cargo packages |
| `package.keywords` | Default `~amd64`. Set `"-*"` for ~arch keywording, or `"amd64"` for stable |

### User config (`~/.config/gentooit/config.yaml`)

Credentials live outside the project so you don't accidentally commit them:

```yaml
# GitHub personal access token (or omit if using GitHub App auth)
github-token: ghp_...

# Git author identity for commits
git-author-name: Your Name
git-author-email: you@example.com

# Optional: hint for the GitHub username (falls back to API lookup)
github-username: your-github-handle

# GitHub App auth (alternative to github-token)
# github-app-id: 123456
# github-app-key: /path/to/private-key.pem
```

### Running the workflows

#### `gentooit propose-downstream`

Generate an ebuild PR for the latest (or pinned) upstream release:

```sh
gentooit propose-downstream
```

Useful flags:

| Flag | Purpose |
|------|---------|
| `--config /path/to/.gentooit.yaml` | Explicit config path (default: walk up from cwd) |
| `--force` | Re-create the ebuild even if one already exists |
| `--no-qa` | Skip `pkgdev manifest` and `pkgcheck scan` |
| `--workdir /tmp/gentooit` | Working directory for clones/distfiles (default: system temp) |

#### `gentooit build`

Run QA checks or a full emerge build against a local package directory:

```sh
# QA only (pkgcheck scan):
gentooit build --mode check --package-dir /path/to/overlay/app-misc/ripgrep

# Full emerge build:
gentooit build --mode build --package-dir /path/to/overlay/app-misc/ripgrep
```

#### `gentooit sync-from-downstream`

Copy your downstream ebuild changes back into the upstream project as a PR:

```sh
# Dry run (preview what would be copied):
gentooit sync-from-downstream --local

# Actually open a PR upstream:
gentooit sync-from-downstream
```

#### `gentooit init`

Scaffold a `.gentooit.yaml` interactively:

```sh
gentooit init --upstream owner/repo --downstream git@github.com:you/overlay.git
```

### What gets generated

For a release `14.1.1` of `BurntSushi/ripgrep` targeting `app-misc`, gentooit
produces in your overlay clone:

```
app-misc/ripgrep/ripgrep-14.1.1.ebuild   # EAPI=8, inherit cargo (for Rust projects)
app-misc/ripgrep/Manifest               # DIST: SIZE + SHA256 + SHA512
app-misc/ripgrep/metadata.xml           # maintainer, bugs-to, remote-id (GLEP 68)
```

The generated ebuild:

- Inherits the **cargo** eclass automatically when the source archive contains a
  `Cargo.toml`
- Adds `DEPEND="dev-lang/rust:="` for cargo projects so the Rust toolchain is
  available at build time
- Detects non-standard source directories and sets `S` accordingly
- Preserves custom `src_*` functions when diff-bumping an existing ebuild

The `Manifest` uses the current Gentoo thin-Manifest policy (`SHA256` +
`SHA512`), so the package is immediately buildable and passes `pkgcheck` without
further edits.

### Rust / cargo packages

gentooit detects Rust projects by looking for `Cargo.toml` inside the downloaded
source archive. When detected:

- `inherit cargo` is added to the ebuild
- `DEPEND="dev-lang/rust:="` is injected (only when you haven't explicitly set
  `DEPEND` in `.gentooit.yaml`)
- The empty `src_install()` block is omitted because the cargo eclass handles
  installation automatically

If your project uses a workspace or has non-standard layout, set `package.depend`
in `.gentooit.yaml` to fine-tune:

```yaml
package:
  depend: "dev-lang/rust:="
```

### Overlay layout tips

- **`gentoo/gentoo`**: works out of the box. Set `downstream[].url` to
  `https://github.com/gentoo/gentoo.git` and `category` / `package_dir` as
  appropriate. You'll need commit rights or to work through your fork.
- **Personal overlay**: standard `git@github.com:you/overlay.git` works
  directly. gentooit clones, branches, commits, and pushes.
- **Local overlay path**: point `downstream[].url` at an absolute local path
  (e.g. `/var/db/repos/gentoo`) for offline use. No network auth needed.
- **Nested overlays**: if your overlay lives under a subdirectory (e.g.
  `ebuilds/app-misc/...`), set `downstream[].package_dir: ebuilds` so gentooit
  writes to the correct location.

### Troubleshooting

**"no releases found"**
- Check `upstream.tag_template` matches your repo's tag format
- Or pin with `upstream.version: 1.2.3`

**"invalid downstream URL"**
- Use SSH (`git@github.com:owner/repo.git`) or HTTPS URLs, not web URLs
- For local paths, use an absolute path (`/home/you/overlay`)

**"pkgdev manifest: not found"**
- Install `app-portage/pkgdev` and `app-portage/pkgcheck` from gentoo/packages
- Or run with `--no-qa` to skip

**PR doesn't open**
- Ensure `open_pull_request: true` (default)
- Verify your token has `repo` scope, or your GitHub App is installed on the
  overlay repo

---

## Contributing to gentooit

### Development setup

```sh
git clone https://github.com/your-org/gentooit.git
cd gentooit

# One-time setup:
rustup default stable
cargo build
```

### Running tests

```sh
cargo test --workspace
cargo clippy --workspace -D warnings
cargo fmt -- --check
```

### Submitting a PR

1. Fork and branch from `main`
2. Make your change, keeping the existing module layout in
   `crates/gentooit-core/src/`
3. Add tests for new behavior in the same file
4. Ensure `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` are clean
5. Open a PR against `main` with a clear description of the problem and solution

### Code style

- Follow existing module conventions (`crates/gentooit-core/src/<module>.rs`)
- Prefer `anyhow::Result` and `thiserror` for errors
- Use `tracing` for logs (not `println!`)
- Keep the CLI thin (`gentooit/src/main.rs`) and put real logic in
  `gentooit-core/`
- Tests live in `#[cfg(test)] mod tests` at the bottom of each module
