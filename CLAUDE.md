# CLAUDE.md — RawZero (working title)

## Communication

**Always reply to the user in Russian.** Code, identifiers, commit messages and code comments are in English. Style: dry, technical, concise, no filler; geometric intuition over formalism when explaining math.

## Goal

An Android camera with no computational photography: **one RAW frame → minimal deterministic pipeline → one JPEG/HEIC** in the gallery. An Android equivalent of Halide's Process Zero.

Requirements:
- Auto exposure and autofocus like a normal camera. The user never has to set anything manually.
- One file per shot in the gallery. DNG is optional and goes to a separate folder.
- The phone must not heat up (the main problem with MotionCam).
- Honest image: grain instead of "watercolor"; no HDR merges, neural nets, beautification or sharpening.

Non-goals:
- Multi-frame merging (HDR+, night mode, multi-frame denoise) — deliberately excluded; this is exactly what we are escaping.
- ML models of any kind.
- `CameraExtensionSession` (vendor HDR/Night) — that is vendor processing.
- Quad-Bayer remosaic to full resolution — also vendor processing (see Pitfalls).

## Target devices

| Device | Known facts |
|---|---|
| Xiaomi 14 (HyperOS 3, Android 16) | Open Camera (Camera2) sees only W and UW on the back; telephoto and front are hidden, presumably by the firmware allow-list (`vendor.camera.aux.packagelist`). 50 MP quad-Bayer sensor. Stock camera Pro-mode DNGs are **not** single frames: at ISO 3200 their shot-noise slope is ~120x below Open Camera's and the noise is spatially correlated (`tools/reports/raw_noise.md`). Open Camera RAW_SENSOR is clean: white noise, dynamic black level preserved with values below it. Its DNGs are lossless-JPEG compressed. |
| Samsung S21 Ultra (One UI) | Open Camera sees only W and UW on the back; 3x/10x telephotos and front are hidden, `physicalCameraIds` is empty. HM3 sensor is nonacell (3×3). Stock Pro-mode DNGs (also from the 3x tele) have black level 0 with negatives clipped to 0, which biases shadows magenta; shading is pre-applied (identity 1x1 GainMap); main-camera noise is spatially correlated. |

**Goal: every physical camera (all rear lenses and front) in RAW, single frame, no vendor processing.** Hidden cameras are not to be accepted as given; mechanisms under investigation: vendor package allow-lists (Xiaomi `vendor.camera.aux.packagelist`, Samsung allow-listed package names), probing camera IDs outside `getCameraIdList()`, logical-camera `CONTROL_ZOOM_RATIO` switching, root as last resort. Never hardcode IDs: discover them on device. Every lens obtained this way must pass `tools/raw_noise.py` (white noise, `a` consistent with ISO) before it counts as "RAW".

## Architecture

```
app/          Kotlin: UI, permissions, Camera2 session, saving to MediaStore
core/         Rust (crate rawzero-core): the entire processing pipeline, no Android dependencies
jni-bridge/   Rust: thin JNI layer (crate `jni`), built with cargo-ndk
desktop/      Rust CLI: runs the pipeline on DNG files for debugging and comparison
tools/        Python (numpy): noise/sharpness analysis, ISP behavior checks
```

- **Kotlin** is a shell only. No pixel processing in Kotlin.
- **Rust `core`** — pure functions over buffers, tested on desktop. CPU first (NEON via autovectorization or `std::simd`); GPU path (wgpu → Vulkan) only after the CPU version is validated.
- **Desktop-first:** every pipeline step is debugged in `desktop/` on real DNGs from the target phones and compared against darktable/RawTherapee. Only validated code moves to the phone.

## Camera: streams and rules

- **Preview:** `PRIVATE` stream → `SurfaceView`, through the hardware ISP. Never build preview from RAW (this is why MotionCam overheats).
- **Capture:** on shutter press, one `TEMPLATE_STILL_CAPTURE` request with `ImageFormat.RAW_SENSOR` output. If `RAW_SENSOR` is unavailable, try `RAW10`/`RAW12` and unpack. Do not use `RAW_PRIVATE`.
- **Support check:** `REQUEST_AVAILABLE_CAPABILITIES` contains `..._RAW`, and the format is listed in `StreamConfigurationMap.getOutputFormats()`.
- **3A** (AE/AF/AWB) stays automatic. Capture the full `CaptureResult` with the frame and pass its metadata to Rust.
- **Resolution:** use the default binned RAW (~12 MP). Do not touch `ULTRA_HIGH_RESOLUTION_SENSOR` / `SENSOR_PIXEL_MODE_MAXIMUM_RESOLUTION`.

## Pipeline (core)

Strictly in this order; each step is a separate function with a test:

1. **Unpack** RAW10/12 → u16 if needed.
2. **Levels:** subtract black level (`SENSOR_DYNAMIC_BLACK_LEVEL`, else `SENSOR_BLACK_LEVEL_PATTERN`, per CFA channel), normalize by `SENSOR_INFO_WHITE_LEVEL` (or `SENSOR_DYNAMIC_WHITE_LEVEL`) → f32 [0,1].
3. **Lens shading:** if `SENSOR_INFO_LENS_SHADING_APPLIED == false`, apply `STATISTICS_LENS_SHADING_CORRECTION_MAP` (request `STATISTICS_LENS_SHADING_MAP_MODE_ON`), bilinearly upsampled to frame size.
4. **Hot pixels:** only if the HAL did not fix them (`HOT_PIXEL_MODE` in the result); median of same-color neighbors.
5. **White balance:** gains from `COLOR_CORRECTION_GAINS` in the result.
6. **Demosaic:** Malvar–He–Cutler first; AHD later as an option. Respect `SENSOR_INFO_COLOR_FILTER_ARRANGEMENT`.
7. **Color:** camera RGB → XYZ(D50) via `SENSOR_FORWARD_MATRIX1/2` (interpolated by color temperature), then XYZ → linear sRGB (Bradford D50→D65).
8. **Tone:** one fixed, gentle global S-curve (no local tone mapping), then sRGB OETF.
9. **Encode:** JPEG (quality ~92) or HEIC, with EXIF and orientation. Optional DNG via `DngCreator` into `Pictures/RawZero/DNG/`.

Forbidden in the pipeline: denoising, unsharp mask or any sharpening, local tone mapping, content-aware "smart" corrections. If ever added, only as an explicit user toggle, off by default.

## Verification (tools/)

- **Noise:** gray wall at high ISO, shot in RAW and in YUV with `NOISE_REDUCTION_MODE_OFF`. Compare noise power spectra: if YUV has the high frequencies cut, the HAL ignores the flag. Reports go to `tools/reports/`.
- **Sharpening:** shoot a sharp edge (ESF), take a luminance profile across it. Overshoot/undershoot = sharpening. Our JPEG must have none.
- **Color:** ColorChecker if available, otherwise compare against darktable on the same DNGs; ΔE per patch.
- **Regression:** reference DNG set in `desktop/testdata/` (git-lfs if over ~50 MB). The CLI must reproducibly produce the same output hashes.

## Performance and heat

- Target: ≤1.5 s per 12 MP frame on CPU on Snapdragon 8 Gen 3 / Exynos 2100. GPU path only if CPU misses the target.
- Process in the background via `WorkManager` or a foreground service. HyperOS aggressively kills background work: write the RAW buffer to a temp file before processing starts so a shot is never lost.
- No continuously running GPU work while the camera is merely open.

## Build

- Rust: `cargo test -p rawzero-core`; desktop CLI: `cargo run -p rawzero-desktop -- input.dng -o out.jpg`.
- Android: `cargo ndk -t arm64-v8a -o app/src/main/jniLibs build --release -p rawzero-jni`, then `./gradlew assembleDebug`.
- `arm64-v8a` only. minSdk 30.
- Python tools run through uv: `uv run --no-project --with rawpy --with numpy tools/<script>.py ...`. The `python` on PATH is an embedded interpreter without pip; do not use it for tools.

## Code style

- Rust: no `unsafe` outside the JNI layer. Pipeline functions are pure: `fn(&Input, &Meta) -> Output`. Metadata lives in one `FrameMeta` struct, filled in Kotlin from `CameraCharacteristics` and `CaptureResult`.
- Kotlin: coroutines, no RxJava. Camera2 directly, no CameraX (CameraX hides the keys we need).
- Formula comments cite their source (paper, DNG spec section, Camera2 key).
- No new dependencies without a reason; justify every new crate.

## Milestones

1. `desktop/`: DNG → JPEG, steps 1–9 on CPU, validated against darktable.
2. Android MVP: preview + shutter + RAW → JNI → JPEG into the gallery. Main camera only.
3. `tools/` scripts and an ISP behavior report for Xiaomi 14 and S21 Ultra.
4. Camera selection (all available IDs), DNG option, EXIF.
5. GPU path (wgpu), only if milestone 2 is too slow.

## Owner

Vegameta — Rust and Godot developer, mathematical modeling (LBM), DevOps. Claude writes the code; architecture decisions and the look of the image are the owner's call.
