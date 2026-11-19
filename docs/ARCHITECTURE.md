# RapidRAW architecture

A map of how an image gets from a file on disk to pixels on screen and back
out as an export, and where each step lives. File paths are relative to the
repo root.

## The two halves

RapidRAW is a [Tauri 2](https://v2.tauri.app) app:

- **Frontend** (`src/`): React 19 + TypeScript, built by Vite, styled with
  Tailwind. It owns the UI, the editing state and the undo history.
- **Backend** (`src-tauri/src/`): Rust. It owns file access, decoding, the GPU
  pipeline, AI models, sidecars and export.

They talk through Tauri commands. The frontend calls
`invoke(Invokes.SomeCommand, args)`, where `Invokes` (`src/types/invokes.ts`)
is the list of command names. On the Rust side each command is a
`#[tauri::command]` function registered in the `generate_handler!` list in
`src-tauri/src/lib.rs`. Both lists are kept by hand, so adding a command
means touching both. The backend pushes progress and state changes back with
`app_handle.emit(...)` events, which the frontend subscribes to in
`src/hooks/useTauriListeners.ts` and similar hooks. Large binary results
(rendered previews, histogram and waveform data) come back as raw IPC
responses or through a `tauri::ipc::Channel` rather than as JSON.

## Frontend state

State lives in zustand stores under `src/store/`:

| Store | Holds |
|---|---|
| `useEditorStore` | the open image, its adjustments, masks, history |
| `useLibraryStore` | current folder, image list, selection, sort and filter |
| `useSettingsStore` | the app settings object mirrored from the backend |
| `useUIStore` | panel visibility, modals, layout |
| `useProcessStore`, `useImmichStore`, `useTetheringStore`, `useCloudStore` | background jobs and integrations |

Behavior is split into hooks in `src/hooks/` (`useImageProcessing`,
`useImageLoader`, `useKeyboardShortcuts`, `useLibraryActions`, ...) so
components in `src/components/` stay mostly presentational. Adjustments are a
JSON object whose defaults are in `src/utils/defaultAdjustments.json`; the
same JSON is what the backend receives and what is stored in sidecars.

## Loading and decoding

`src-tauri/src/decode/` turns a file into a linear image:

1. `formats.rs` decides whether a file is RAW or a standard format by
   extension.
2. RAW files are decoded with [`rawler`](https://github.com/CyberTimon/RapidRAW-DngLab),
   a RapidRAW fork of dnglab pulled in as a git dependency. On macOS,
   `apple_raw.rs` can use Core Image's RAW filter instead.
3. `demosaic.rs` and `develop.rs` run demosaicing and the initial develop
   step on the GPU (`shaders/demosaic.wgsl`, `shaders/develop.wgsl`), with CPU
   implementations in `demosaic/cpu.rs` and `develop/cpu.rs` used when no
   suitable GPU is available (`gpu_fallback.rs`).
4. White balance, baseline exposure and as-shot metadata are read alongside
   (`white_balance.rs`, `baseline_exposure.rs`, `as_shot.rs`).

Non-RAW images go through the `image` crate plus dedicated decoders for JPEG
XL and others. Decoded images are cached in memory (`AppState` in
`app/state.rs`), and neighbouring images are preloaded (`image_preload.rs`).

## The render pipeline

`src-tauri/src/render/` applies the user's adjustments:

- `uniforms.rs` converts the adjustment JSON into the uniform structs the
  shaders read. This is the contract between the JSON keys the frontend
  writes and the GPU code.
- `gpu/context.rs` creates and caches the wgpu device and queue.
- `gpu/pipelines.rs` builds the compute pipelines from the WGSL sources in
  `src-tauri/src/shaders/`, embedded at compile time with `include_str!`.
  `shader.wgsl` is the main per-pixel adjustment shader; `blur.wgsl`,
  `guided.wgsl`, `lens_blur.wgsl`, `motion_blur.wgsl`, `fog.wgsl`,
  `relight.wgsl` and `light_effects.wgsl` implement multi-pass effects, and
  `transform.wgsl` handles geometry.
- `gpu/processor.rs` runs the passes, including masks (`src-tauri/src/masks/`)
  and LUTs, and reads back the result.
- `geometry.rs` handles crop, rotation, perspective and lens correction
  (lens profiles from the bundled lensfun database in `src-tauri/lensfun_db/`).
- `preview_worker.rs` runs editor previews on a dedicated thread. Requests
  arrive over a channel and only the latest request per layer is rendered, so
  a burst of slider movements produces one render rather than a queue.
- `gpu/display.rs` is the optional wgpu display path that draws the preview
  straight to a window surface, with color management and HDR output
  (`platform/monitor.rs` reads display profiles). Without it the preview is
  encoded (`preview_jpeg.rs`) and shown by the webview.
- `analytics.rs` computes histograms and waveforms off the render thread.

Expensive intermediate stages (AI patches, warped geometry) are cached by a
hash of the inputs that affect them (`preview.rs`, `cache_utils.rs`), so
changing a later adjustment does not recompute earlier stages.

## AI features

`src-tauri/src/ai/` runs ONNX models through `ort` (loaded dynamically).
Models are downloaded on demand and cached (`models.rs`); `base_cache.rs`
keeps per-image model outputs. Features include subject and sky masking,
denoise, upscaling, inpainting (`lama.rs`, `retouch/`), and tagging
(`library/tagging/`). `connector.rs` talks to an optional external AI
server.

## Sidecars and metadata

Edits are non-destructive and stored next to the image:

- `IMG_0001.CR3.rrdata` is a JSON sidecar (`metadata/sidecar.rs`,
  `ImageMetadata`): format version, rating, pick/reject flag, the adjustment
  JSON, tags (including `color:` labels) and cached EXIF.
- Virtual copies share the image file and get their own sidecar,
  `IMG_0001.CR3.<6 hex digits>.rrdata`. Inside the app a virtual copy is
  addressed as `path?vc=<id>` (`library/sidecars.rs`, `parse_virtual_path`).
- Sidecar writes are atomic (`library/file_ops.rs`, `write_file_atomic`) and a
  sidecar that fails to parse is quarantined rather than overwritten.
- Ratings, labels and keywords can be mirrored to and from `.xmp` files
  (`metadata/xmp.rs`) when XMP sync is enabled.
- Settings are a JSON file in the app data directory (`app/settings.rs`),
  read with per-field defaults so older or partially invalid files still load.

## Library and export

`src-tauri/src/library/` lists folders, generates and caches thumbnails,
manages albums, presets, culling and file operations (copy, move, rename with
templates). `src-tauri/src/export/` renders full-resolution images through
the same GPU pipeline, encodes them (JPEG via mozjpeg, PNG, TIFF, WebP, JPEG
XL, HDR gain maps), applies naming templates and watermarks, and writes
metadata (`metadata/export_metadata.rs`).

## Platform code

`src-tauri/src/platform/` holds per-OS window handling, monitor and ICC/HDR
detection, the Wayland subsurface used by the wgpu display on Linux, and the
Android bridge. `src-tauri/src/integrations/` contains Immich sync, camera
tethering through libgphoto2 (the `tethering` cargo feature, macOS and Linux
only) and "edit in external app".

## CI

Workflows in `.github/workflows/`:

- `lint.yml`: frontend type check, lint and format; `cargo fmt`, `cargo
  clippy`, `cargo test` and `cargo deny` on Linux.
- `pr-ci.yml` / `ci.yml`: full app builds for every platform through the
  reusable `build.yml`.
- `release.yml`: release builds attached to a GitHub release.

All third-party actions are pinned to commit SHAs, and the Rust toolchain
version in the workflows must match `src-tauri/rust-toolchain.toml`.
