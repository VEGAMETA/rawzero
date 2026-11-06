# CLAUDE.md — RawZero (working title)

## Communication

**Always reply to the user in Russian.** Code, identifiers, commit messages and code comments are in English. Style: dry, technical, concise, no filler; geometric intuition over formalism when explaining math.

## Goal

An Android camera for two people (Xiaomi 14 primary, Samsung S21 Ultra as test/reference/second platform) that takes **good, natural photos from good camera hardware, with no AI**: one shot → deterministic classical pipeline → one JPEG in the gallery, looking like what the screen showed when shooting. RAW (DNG) is an option, not the default.

Three modes (owner decision, 2026-11-06):
1. **Classic** (default, main focus). Single frame. A classical in-camera JPEG engine: noise reduction driven by the sensor noise profile (strong on chroma, gentle on luma), mild sharpening, global tone curve plus gentle single-frame shadow/highlight compression. Global look (exposure, white balance, contrast, saturation) matched to the ISP preview the user saw.
2. **Process Zero**. Single frame, minimal pipeline: levels → shading → WB → demosaic → color → fixed global curve. No NR, no sharpening, no local operators; grain stays.
3. **Night**. Burst of RAW frames, aligned and merged with a classical, non-ML method (HDR+-style, Hasinoff et al. 2016), then the Classic engine. Only on explicit user choice. (Not implemented yet: currently one Classic frame.)
4. **Screen** ("Экран"). The ISP's full-resolution still JPEG (good detail) recolored to the preview: the HAL tunes preview and still differently (Xiaomi still is cooler/flatter), so per-channel histogram specification maps the still onto the preview frame grabbed at the shutter (`ScreenLook`). Front mirrored, EXIF rotation written by us (the Xiaomi HAL writes none).

One photo per shot: front lens mirrored like the preview, rear normal. DNG only in RAW mode; dev pairs (DNG + ISP JPEG) are an opt-in toggle. Verified WYSIWYG on Xiaomi 14: the saved photo shown in the gallery and the preview before the shutter differ by ~1/255 in mean RGB on screen.

Requirements:
- Auto exposure and autofocus like a normal camera. The user never has to set anything manually.
- One file per shot in the gallery. DNG is optional and goes to a separate folder.
- The phone must not heat up (the main problem with MotionCam). Preview stays on the hardware ISP.
- All cameras: every rear lens and the front camera (front matters for the users).

Non-goals:
- ML models of any kind (no neural denoise, segmentation, face retouch, "AI scene").
- `CameraExtensionSession` (vendor HDR/Night) and vendor-processed stock DNGs — that is vendor processing.
- Content-aware "smart" corrections (sky/face detection, beautification).
- Quad-Bayer remosaic to full resolution — vendor processing.

## Target devices

| Device | Known facts |
|---|---|
| Xiaomi 14 (HyperOS 3, Android 16) | `getCameraIdList()` returns only logical 0 (W+UW) and front 1; tele is hidden (reachable by direct ID, see below). No `vendor.camera.aux.packagelist` property on this firmware. 50 MP quad-Bayer sensor. Stock camera Pro-mode DNGs are **not** single frames: at ISO 3200 their shot-noise slope is ~120x below Open Camera's and the noise is spatially correlated (`tools/reports/raw_noise.md`). Open Camera RAW_SENSOR is clean: white noise, dynamic black level preserved with values below it. Its DNGs are lossless-JPEG compressed. |
| Samsung S21 Ultra (One UI) | Open Camera sees only W and UW on the back; 3x/10x telephotos and front are hidden, `physicalCameraIds` is empty. HM3 sensor is nonacell (3×3). Stock Pro-mode DNGs (also from the 3x tele) have black level 0 with negatives clipped to 0, which biases shadows magenta; shading is pre-applied (identity 1x1 GainMap); main-camera noise is spatially correlated. |

**Goal: every physical camera in RAW, single frame, no vendor processing.** Probe results (`app/` probe, `tools/camera_dump.py`, `tools/reports/raw_noise_probe.md`), 2026-11-06:

| | W | UW | Tele | Tele 2 | Front |
|---|---|---|---|---|---|
| S21U (One UI, Android 15) | 0 | 2 | 50 (9 mm, 3x) | 52 (30.6 mm, 10x) | 1/3/4: no RAW in HAL at all |
| Xiaomi 14 (HyperOS 3, Android 16) | 2 (also logical 0) | 3 (also physical stream of logical 0) | 4 (9 mm) | — | 1 |

- **Samsung:** cameraserver hides IDs 20/21/23/50/52/56/58 per package. Under the package names `com.samsung.android.scan3d` or `com.samsung.android.ruler` (app flavors `scan3d`, `ruler`) they are visible and 50/52 deliver RAW_SENSOR. Physical streams through logical 20/21/23 fail; use the direct IDs. RAW is 10-bit with black level 0 and negatives clipped to 0 by the HAL (shadow bias, unrecoverable). Noise is white (single frame).
- **Xiaomi:** no allow-list for this; hidden IDs 2/3/4 answer `getCameraCharacteristics`/`openCamera` from any package, `getCameraIdList()` just omits them. Opening hidden logical ID 5 crashed the camera provider: never open hidden logical cameras. HAL reports `LENS_SHADING_APPLIED=true` but the RAW is not shading-corrected (noise test, visual check): the GainMap must be applied.
- Discover lenses on device, never hardcode (`CameraCatalog`): probe IDs 0..255 plus physical members of listed logical cameras, verify each by opening it and configuring the real stream set (preview + RAW + JPEG, then without JPEG), cache per `Build.FINGERPRINT`, rescan button in settings. Lenses without RAW stay usable as ISP-only (saved as the ISP JPEG, marked "·ISP"). Without an ISP JPEG stream, Classic uses the preview frame at the shutter as the look reference. Every lens must pass `tools/raw_noise.py` (white noise, `a` consistent with ISO, `corner` ratio for pre-applied shading) before it counts as "RAW".
- **Long exposure:** standard `SENSOR_INFO_EXPOSURE_TIME_RANGE` caps at 1 s (Xiaomi rear), 0.41 s (Xiaomi front), 0.1 s (Samsung); no Android 16 AE priority modes. Xiaomi: Qualcomm `org.codeaurora.qcamera3.iso_exp_priority` (`select_priority=1`, `use_iso_exp_priority=<ns>`) gives real exposures to 30 s (UW/tele) / 201 s (W): capture time matches and RAW signal grows (2 s 1.6x, 8 s 3.3x of 1 s), but the HAL picks the ISO itself, so result metadata ISO/exposure must not be trusted blindly; verify with RAW signal. Samsung has `samsung.android.sensor.info.exposureTimeRange` up to 30 s, request key unknown. Preview never runs long exposures: <=1/30 s (30 fps) showing the still's real exposure x ISO after its ISO clamp, sensor ISO first, then `CONTROL_POST_RAW_SENSITIVITY_BOOST` (ISP-only gain, does not touch RAW; 1x in every other request).
- `adb install` on HyperOS needs "Install via USB" enabled and the screen on and unlocked (otherwise it is silently cancelled); `-g` is rejected, grant CAMERA with `pm grant`. Verify with `dumpsys package <pkg> | grep lastUpdateTime`.

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
- **Camera IDs:** see Target devices; hidden IDs are reached by direct ID (Xiaomi) or allow-listed package name (Samsung).
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
8. **Tone:** global curve then sRGB OETF. Process Zero: one fixed gentle S-curve. Classic: curve and exposure fitted to the ISP output of the same moment (see below), plus gentle single-frame local tone compression.
9. **Encode:** JPEG (quality ~92) or HEIC, with EXIF and orientation. Optional DNG via `DngCreator` into `Pictures/RawZero/DNG/`.

Classic adds, each a separate pure function with a test, all parameters derived from metadata (noise profile, ISO), never from image content classification:
- **Noise reduction** (`core/src/denoise.rs`) in the raw domain, after levels and before shading/WB/demosaic: each Bayer cell -> Y, Cr, Cb, Gd; Poisson-Gaussian model fitted blindly from Gd (no reliance on HAL noise profiles); Y by 8x8 Walsh-Hadamard hard threshold + empirical Wiener, stride 2, Kaiser aggregation; chroma by a guided filter on the denoised Y; `LUMA_GRAIN_KEEP` of the grain kept. Parallel (rayon) with fixed-order accumulation, so output hashes do not depend on thread count.
- **Sharpening**: mild luma-only unsharp mask, amount scaled down with noise.
- **Local tone**: edge-aware base/detail split of log luminance (guided filter), gentle base compression; no halos (verified on edges, `tools/`).
- **Look match** (`core/src/look.rs`): the preview frame grabbed at the shutter (PixelCopy; the ISP still only as fallback) is the reference; a full 3x3 chromaticity matrix (grid cells normalized to unit luminance, smooth mid-tone cells, ridge toward diagonal; paired through the reference geometry: preview = sensor image rotated by SENSOR_ORIENTATION and mirrored for the front lens, verified by luminance correlation 0.97-0.99 vs <0.35 for wrong orientations; falls back to WB gains when correlation < 0.85, e.g. different FOV), a monotone tone curve (luminance histogram specification) and a saturation factor (mean mid-tone chroma) are fitted from our render to it. Chroma noise uses transform-domain shrinkage per plane plus a small guided filter: a large guided filter averaged equal-luminance regions into one hue ("sepia"). Distribution-based (no registration), global, low-parameter; detail and noise handling are ours.

Process Zero forbids all of the above.

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

1. `desktop/`: DNG → JPEG, steps 1–9 on CPU (done: Process Zero baseline; darktable comparison pending).
2. Capture tool in `app/`: ISP preview, all cameras (discovered IDs), shutter → DNG + ISP JPEG of the same request into `Pictures/RawZero/dev/`. Builds the reference dataset for the look work.
3. Classic engine in `core`, desktop-first, evaluated against the ISP references (noise, sharpness, look match).
4. Android MVP: JNI, mode switch (Classic / Process Zero), JPEG into the gallery, optional DNG, EXIF.
5. Night mode (burst align + merge).
6. GPU path (wgpu), only if CPU misses the time target.

## Owner

Vegameta — Rust and Godot developer, mathematical modeling (LBM), DevOps. Claude writes the code; architecture decisions and the look of the image are the owner's call.
