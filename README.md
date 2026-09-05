# gentooit

**packit for Gentoo Linux.** Automate packaging upstream projects into Gentoo
ebuilds — the same way [packit](https://github.com/packit/packit) automates
packaging upstream projects into Fedora.

gentooit has two parts:

- **CLI** (`gentooit`) — run locally or in CI to propose ebuilds, run QA, and
  sync packaging files.
- **Service** (`gentooit-service`) — a GitHub App that watches upstream releases
  and downstream PRs, then runs the workflows automatically. This is the
  differentiator: once configured, new upstream releases turn into downstream
  PRs without anyone touching a keyboard.

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

The service handles the full loop:

1. A new upstream release fires a webhook
2. gentooit-service downloads the archive, generates the ebuild + Manifest +
   metadata.xml, runs `pkgcheck` / `pkgdev` if available
3. It opens a PR against the downstream overlay
4. On downstream PRs, it runs build/QA and posts the results as a comment

No manual `propose-downstream` invocation required.

## Workflows

gentooit mirrors the packit workflow set for Gentoo:

| gentooit command            | Packit equivalent        | What it does |
| --------------------------- | ------------------------ | ------------ |
| `gentooit propose-downstream` | `packit propose-downstream` | Take the latest (or pinned) upstream release, derive the source archive, build the ebuild + `Manifest` + `metadata.xml`, clone the downstream overlay, create a branch, commit, and open a PR. |
| `gentooit build`            | `packit build`           | Run `pkgcheck scan` (QA) and/or `emerge` build an ebuild in the downstream check-out. Also emits a GitHub Actions workflow that builds in a `gentoo/stage3` container. |
| `gentooit sync-from-downstream` | `packit sync-from-downstream` | Copy packaging files from the downstream ebuild repository back into the upstream project via a PR. |
| `gentooit init`             | `packit init`            | Scaffold a `.gentooit.yaml` project config. |

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

## Deploying the service (hands-off automation)

The CLI is great for one-offs, but the real leverage comes from running
`gentooit-service` as a GitHub App. Once deployed, the service watches your
upstream repos and downstream overlays and opens PRs automatically.

### What the service does

| Webhook event | Action |
|---------------|--------|
| `release: published` on upstream | Downloads the archive, generates ebuild + Manifest + metadata.xml, runs QA if tools are present, opens a PR against the downstream overlay |
| `pull_request: opened / synchronize` on downstream | Clones the overlay, runs `pkgcheck scan` / `pkgdev manifest`, posts results as a PR comment |

Everything runs in the background (`tokio::spawn`), so the webhook returns
immediately and the heavy work happens out-of-band.

### 1. Create a GitHub App

1. Go to **Settings → Developer settings → GitHub Apps → New GitHub App**
2. Name: `gentooit` (or whatever you prefer)
3. **Webhook**:
   - URL: `https://your-host:3000/` (or wherever you deploy)
   - Secret: generate a random string, save it as `GENTOOIT_WEBHOOK_SECRET`
4. **Permissions**:
   - Pull requests: **Read & write**
   - Contents: **Read & write** (to fetch `.gentooit.yaml` and post comments)
   - Metadata: **Read-only** (always selected)
5. **Subscribe to events**:
   - Release
   - Pull request
6. Create the app. Note the **App ID** and download the **Private key** (PEM file)

### 2. Install the App on your repos

1. In the GitHub App settings, click **Install App**
2. Choose the organization/account
3. Select the upstream repos (to watch releases) and downstream overlay repos
   (to open PRs and post comments)
4. The installation ID is resolved automatically per-repo at runtime

### 3. Run the service

```sh
export GENTOOIT_APP_ID=123456
export GENTOOIT_APP_KEY=/path/to/private-key.pem
export GENTOOIT_WEBHOOK_SECRET=your-webhook-secret

gentooit-service --port 3000
```

For production, run it behind a reverse proxy (nginx, Caddy) with TLS. GitHub
requires HTTPS webhooks unless you use `ngrok` for testing.

### 4. Configure your project

The service fetches `.gentooit.yaml` from the upstream repo root at runtime.
No per-service config needed — just add the project config to your upstream
repo:

```yaml
spec_version: "1.0"
upstream:
  vcs: github
  upstream: BurntSushi/ripgrep
  package_name: ripgrep
  tag_template: "v{version}"
package:
  description: Recursively search directories for a regex pattern
  homepage: https://github.com/BurntSushi/ripgrep
  license: Unlicense
  keywords: "~amd64"
  maintainer_email: you@example.com
downstream:
  - url: git@github.com:gentoo/gentoo.git
    branch: master
    category: app-misc
open_pull_request: true
```

That's it. Push a new release upstream and gentooit-service will open a PR
downstream.

### 5. Systemd service (optional)

```ini
[Unit]
Description=gentooit-service
After=network.target

[Service]
Type=simple
User=gentooit
WorkingDirectory=/opt/gentooit
ExecStart=/opt/gentooit/target/release/gentooit-service --port 3000
Environment="GENTOOIT_APP_ID=123456"
Environment="GENTOOIT_APP_KEY=/etc/gentooit/private-key.pem"
Environment="GENTOOIT_WEBHOOK_SECRET=your-webhook-secret"
Restart=always

[Install]
WantedBy=multi-user.target
```

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
- [x] `sync-from-downstream` local mode that copies files into the upstream
  worktree and commits

## Security

We run `cargo audit` in CI and address high/critical vulnerabilities
promptly. One advisory is currently accepted as a known risk:

- **RUSTSEC-2023-0071** (`rsa` 0.9.10, medium) — Marvin Attack timing
  sidechannel. This is a transitive dependency through `jsonwebtoken` 10
  (used by `octocrab` for GitHub App JWT signing). The `rsa` crate maintainers
  have not yet published a patched release. We monitor this dependency and will
  upgrade as soon as a fix is available.

## Maintenance

- **CI** (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and `cargo audit` (RUSTSEC).
- **Renovate** (`renovate.json`) keeps Cargo dependencies up to date. Note the deliberate coupling rules: `octocrab`/`jsonwebtoken` are grouped (their majors must track each other), `hmac`/`sha2` are paired (the service pins `sha2 0.10` to match `hmac 0.12`'s digest), and `git2` major bumps require manual approval).

## License

MIT
