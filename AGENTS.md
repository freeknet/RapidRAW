# RapidRAW: notes for coding agents

RapidRAW is a GPU-accelerated, non-destructive RAW photo editor built with
Tauri 2. The frontend is React + TypeScript (Vite, Tailwind, zustand); the
backend is Rust (edition 2024) in `src-tauri/`, with image processing on the
GPU through wgpu compute shaders. See `docs/ARCHITECTURE.md` for how the
pieces fit together and `CONTRIBUTING.md` for the human-facing version of
these rules. `CLAUDE.md` holds the same content as this file for tools that
read that name instead; keep the two in sync.

## Commands

From the repo root:

| What | Command |
|---|---|
| Install frontend deps (exact lockfile) | `npm ci` |
| Run the app in dev mode | `npm run start` (`npm run start:tethering` adds camera tethering, macOS/Linux only) |
| Type check | `npm run typecheck` |
| ESLint | `npm run lint` |
| Prettier check | `npm run format:check` (`npm run format` to fix) |
| Translation sync | `npm run i18n:check` (`npm run i18n:extract` after adding strings) |

From `src-tauri/`:

| What | Command |
|---|---|
| Format check | `cargo fmt --check` |
| Lint (what CI runs) | `cargo clippy --all-targets --all-features -- -D warnings` |
| Tests | `cargo test` |
| Dependency audit | `cargo deny check` (`cargo install cargo-deny --locked` once) |
| Fast optimized local build | `cargo build --profile release-fast` |

The Rust toolchain is pinned in `src-tauri/rust-toolchain.toml`; rustup picks
it up automatically. The workflows name the same version, and
`src-tauri/tests/toolchain_pin.rs` fails if they drift apart.

`--all-features` includes `tethering`, which needs libgphoto2 and builds only on
macOS and Linux. On Windows, run clippy without it:
`cargo clippy --all-targets -- -D warnings`.

## Handoff gate

Before calling a change done, run every check that covers what you touched
and make sure it passes:

- Rust: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` (from `src-tauri/`).
- Frontend: `npm run typecheck`, `npm run lint`, `npm run format:check`, `npm run i18n:check`.
- Workflows or `rust-toolchain.toml`: `cargo test --test toolchain_pin`.
- Dependencies: `cargo deny check`.

If a check cannot run on your machine (for example the tethering feature on
Windows), say so explicitly instead of reporting it as passing.

## Git

- Commit only the paths you changed: `git commit -- <paths>`. Never
  `git add -A` or `git add .`; other work may be in progress in the same tree.
- Commit messages follow the upstream style: lowercase plain sentences that
  describe the change, for example
  `keep a full size crop full size and in place when switching aspect ratios`.
  No conventional-commit prefixes (`feat:`, `fix:`, `chore:`), no trailing
  period required.
- Don't rewrite published history or force-push shared branches.

## Where things live

Backend, `src-tauri/src/`:

- `lib.rs`: app setup, state registration and the `invoke_handler` list of
  every Tauri command.
- `app/`: settings (`settings.rs`), shared state, logging, storage and cache
  limits, launch arguments.
- `decode/`: file loading, RAW decode via `rawler`, demosaic and develop
  (GPU with a CPU fallback), supported formats.
- `render/`: the editing pipeline. `uniforms.rs` maps adjustment JSON to GPU
  uniforms, `gpu/` holds the wgpu context, pipelines, processor and display
  surface, `preview_worker.rs` runs editor previews on a worker thread.
- `shaders/`: WGSL compute and display shaders, embedded with `include_str!`.
- `adjustments/`, `masks/`, `effects/`, `retouch/`, `merge/`: individual
  feature areas (lens correction, LUTs, masks, blur effects, healing, HDR and
  panorama merge, focus stacking).
- `ai/`: ONNX models through `ort` (masking, denoise, upscale, inpainting,
  tagging), model download and caching.
- `library/`: folder listing, thumbnails, sidecars, file operations,
  albums, tagging, culling, presets.
- `metadata/`: `.rrdata` sidecar format, EXIF, XMP sync, export metadata.
- `export/`: export pipeline, encoders, naming, watermarks.
- `integrations/`: Immich, tethering, edit-in-external-app.
- `platform/`: window, monitor/ICC/HDR and Android specifics.

Frontend, `src/`:

- `App.tsx` / `main.tsx`: root component and bootstrap.
- `store/`: zustand stores (`useEditorStore`, `useLibraryStore`,
  `useSettingsStore`, `useUIStore`, ...).
- `hooks/`: behavior split out of components (image loading, processing,
  shortcuts, Tauri event listeners).
- `components/`: `panel/`, `views/`, `modals/`, `adjustments/`, `ui/`, etc.
- `types/invokes.ts`: the `Invokes` enum of backend command names. Keep it
  in sync with the command list in `src-tauri/src/lib.rs`.
- `i18n/locales/`: translations; every user-visible string goes through i18next.
- `utils/`: shared helpers, including `defaultAdjustments.json`.

## Conventions

- Settings structs use per-field `#[serde(default)]` so older settings files
  keep loading; add new fields the same way.
- Don't add `unsafe` without a `// SAFETY:` comment explaining the invariant;
  clippy denies undocumented unsafe blocks.
- `todo!()` and `dbg!()` are denied by clippy; don't commit them.
