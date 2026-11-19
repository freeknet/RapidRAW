# Contributing to RapidRAW

Thanks for helping out. This page covers how to build the app, what to check
before opening a pull request, and how commits are written here. For a tour of
the code, read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Setup

You need:

- Node.js 22 and npm.
- Rust through [rustup](https://rustup.rs). The exact toolchain is pinned in
  `src-tauri/rust-toolchain.toml` and rustup installs it on first use.
- The [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for
  your OS. On Debian/Ubuntu that is roughly
  `libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev pkg-config`.
- Optional, for camera tethering on macOS/Linux: libgphoto2
  (`brew install libgphoto2 pkg-config` or `apt install libgphoto2-dev`).

Then:

```sh
npm ci
npm run start            # dev build with hot reload
npm run start:tethering  # same, with the tethering feature
```

For an optimized build that still rebuilds quickly, use the `release-fast`
cargo profile (`cargo build --profile release-fast` in `src-tauri/`). The
`release` profile uses fat LTO and is what ships; it is slow to rebuild.

## Before opening a pull request

CI runs these, so running them locally saves a round trip.

Frontend, from the repo root:

```sh
npm run typecheck
npm run lint
npm run format:check
npm run i18n:check
```

Backend, from `src-tauri/`:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check   # after `cargo install cargo-deny --locked`
```

`--all-features` includes `tethering`, which only builds on macOS and Linux.
On Windows, drop `--all-features`.

Some rules the checks enforce:

- Every `unsafe` block needs a `// SAFETY:` comment that explains why the
  operation is sound.
- `todo!()` and `dbg!()` don't get committed.
- User-visible strings go through i18next; add new keys with
  `npm run i18n:extract`.
- New settings fields need `#[serde(default)]` so older settings files keep
  loading.

## Commits and pull requests

- Write commit messages as lowercase plain sentences that describe the
  change, matching the existing history, for example
  `show the file naming template for single image exports`. No `feat:` /
  `fix:` prefixes.
- Keep a pull request to one topic. Fill in the pull request template,
  including how you tested the change.
- Larger changes to repo-wide policy (CI, lints, dependencies) are easier to
  land if you open a short issue first to discuss them.

## Dependencies

Dependabot opens weekly update PRs for cargo, npm and GitHub Actions, and the
`cargo deny` job checks advisories, licences and sources on every PR. Policy
for both lives in `src-tauri/deny.toml` and `.github/dependabot.yml`. GitHub
Actions are pinned to full commit SHAs; update them through Dependabot or by
resolving the new tag's commit, keeping the `# vX.Y.Z` comment in sync.
