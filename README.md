# VIctory for RTX 20-Series (6 GB+)

An optimized Windows Direct3D 12 compatibility runtime and RDNA 2-to-DXIL shader translator tuned for **NVIDIA RTX 20-series GPUs** (including the **RTX 2060 6 GB**), while scaling automatically to 8 GB, 12 GB, and larger cards.

> **No Game Files Included:** This repository contains runtime, HLE, and shader-translation source code only. You must provide your own legal copy of the title outside the repository. See [docs/asset-policy.md](docs/asset-policy.md).

---

## Problems and Fixes

### 1. VRAM Exhaustion and PCIe Paging Stutters on 6 GB Cards
- **Problem:** Fixed allocation targets (such as a hardcoded 3,072 MiB target reserve and multi-gigabyte pool/floor assumptions), eager full mip-chain uploads, and untracked concurrent texture creations quickly exceeded 6 GB dedicated VRAM. Once overflowed to shared system memory over PCIe, frame rates collapsed into single digits.
- **Fix:**
  - Scaled texture residency floors (`256 MiB` on `<= 6.5 GiB` VRAM, `512 MiB` on `<= 9 GiB`, `1024 MiB` on `<= 12 GiB`) and target headroom (`1280 MiB` on 6 GB, `2048 MiB` on 8 GB, `3072 MiB` on higher VRAM) using live DXGI local video memory queries and in-flight upload byte tracking (`PendingTextureGuard` in `crates/victory-gpu/src/resource.rs`).
  - Enabled on-demand tiled mip allocation (`VICTORY_LEVELS_ON_DEMAND`, on by default) so streamed textures commit only actively viewed mip levels (`crates/victory-gpu/src/tiles.rs`).
  - Scaled staging pool lookahead, idle pool retention, and GPU-local card copy limits to reported VRAM capacity (`crates/victory-gpu/src/pool.rs`, `crates/victory-gpu/src/shadow.rs`).

### 2. Wrong Texture Bindings, Descriptor Heap Limits, and sRGB Format Mismatches
- **Problem:** Reused bindless descriptor slots could lag behind streaming uploads or miss explicit sRGB/linear view casts on filled and unordered-access textures. On NVIDIA drivers with a 1,000,000-slot view heap limit, preserving headroom for ~720k long-lived title views is required alongside dynamic per-draw tables.
- **Fix:**
  - Enabled async ahead-of-draw texture uploads and non-blocking refreshes (`VICTORY_UPLOAD_AHEAD`, `VICTORY_TEXTURE_NO_WAIT`, `VICTORY_SHOW_AFTER` in `crates/victory-gpu/src/descriptor/host.rs`, `texture.rs`, `texture/ahead.rs`) while keeping `VICTORY_KEEP_SHOWN=1`.
  - Preserved the full static title view range (`2^18` dynamic slots, ~737,855 title slots) on capped NVIDIA heaps while allowing explicit tuning via `VICTORY_DYNAMIC_SLOTS`, and scaled dynamic headroom on uncapped hosts (`crates/victory-gpu/src/descriptor/host.rs`).
  - Resolved default SRVs and UAVs on typeless resources to concrete formats (`default_srv_for_desc`, `default_uav_for_desc`), expanded `SRGB` pairs to 7 formats (`R8G8B8A8`, `B8G8R8A8`, `B8G8R8X8`, `BC1`, `BC2`, `BC3`, `BC7`), backed sRGB-family filled textures and sRGB UAV textures as typeless (`family`), and mapped typeless formats to concrete formats (`footprint_desc`) before `GetCopyableFootprints` calls (`crates/victory-gpu/src/resource.rs`, `texture.rs`, `texback.rs`, `capture.rs`).

### 3. Black `NaN` Cutouts Inside Bright Highlights and Bloom
- **Problem:** Console RDNA 2 shaders run with `DX10_CLAMP` semantics, flushing `NaN` results to `+0.0` on saturated operations. Lowering `clamp` to a bare DXIL `Saturate` intrinsic on NVIDIA GPUs allowed `NaN` pixels (from `0 * inf`, negative `rsq`/`log`, or divisions) to survive and spread across bloom mip downsamples, leaving sharp pitch-black holes inside bright doorways, specular highlights, and sky flares.
- **Fix:**
  - Added integer-bit NaN detection (`is_nan_f32`) to flush `NaN` to `+0.0` in `Lower::saturate`, updated all floating-point `clamp` lowering paths to use it, and added full `omod`/`clamp` handling to `v_cvt_pkrtz_f16_f32` (`crates/victory-shader/src/emit/lower.rs`, `mod.rs`), alongside legacy fused `f32` multiply-add lowering.
  - Added opt-in filtering for strongly magenta indirect probe history (`VICTORY_AMBIENT_FILTER_MAGENTA=1` in `crates/victory-gpu/src/ambient.rs`).

### 4. Display Precision, Dark Gradient Banding, and Series S Two-Plane Composition
- **Problem:** Series S presentation composited into an 8-bit `DXGI_FORMAT_R8G8B8A8_UNORM` swap chain regardless of whether the game's scene plane used 10-bit (`R10G10B10A2_UNORM`) or 16-bit float (`R16G16B16A16_FLOAT`), while `compose::view` kept `_SRGB` SRV formats when writing to a non-sRGB swap chain—causing unnecessary quantization banding on dark gradients and double-darkened shadows.
- **Fix:**
  - Preserved `swap_format(desc.Format)` precision during Series S two-plane composition (`crates/victory-gpu/src/host.rs`) and mapped `_SRGB` display planes to non-decoded `UNORM` views matching single-plane presentation (`crates/victory-gpu/src/compose.rs`).

### 5. Wave32 Water Rendering and Geometry Bounds
- **Problem:** Console 64-lane water tile append shaders and water volume consumers split into two 32-lane waves on NVIDIA Wave32 GPUs, corrupting water tile indices and water lighting/color. Unbounded vertex buffer views and incomplete indexed `RECTANGLES` (`R10G10B10A2_UNORM`) attributes also caused stretched polygons.
- **Fix:**
  - Added the Wave32 water tile generator append rewrite (`crates/victory-shader/src/emit/water_tiles.rs`) with opt-in `VICTORY_WAVE_WIDE=1` widening while avoiding forced loop-widening barriers (`crates/victory-shader/src/emit/mod.rs`).
  - Added contiguous vertex-buffer shadow validation (`resolve_geometry_shadow` in `crates/victory-gpu/src/draw.rs`) and `R10G10B10A2_UNORM` fourth-corner vertex attribute expansion (`crates/victory-gpu/src/primitive.rs`).

### 6. CPU Translation Overhead and System RAM Pressure
- **Problem:** Single-threaded command translation, per-draw temporary heap allocations, redundant D3D12 copy/dispatch overhead, and large fixed DirectStorage staging buffers reduced frame pacing and strained 16 GB host systems.
- **Fix:**
  - Enabled ThinLTO (`lto = "thin"` in [Cargo.toml](Cargo.toml)), multi-threaded thin list translation (`list/thin.rs`), balanced descriptor compare worker counts (`land.rs`), joined command-list parts and indirect batching defaults (`host.rs`, `draw.rs`), layout-aligned staging (`host/send.rs`), thread-local scratch reuse (`draw.rs`, `host/place.rs`, `host/send.rs`, `dstorage.rs`), and RAM-scaled DirectStorage staging floors (`crates/victory-hle/src/modules/dstorage.rs`).

---

## Requirements

- **OS & CPU:** Windows 10/11 64-bit with AVX2 support
- **GPU:** Direct3D 12 GPU supporting Shader Model 6.6 and Mesh Shaders (e.g., RTX 2060, 2070, 2080)
- **Toolchain (for building from source):** Rust toolchain specified in [rust-toolchain.toml](rust-toolchain.toml), MSVC C++ Build Tools, and Windows SDK

---

## Building from Source

```powershell
git clone https://github.com/imyuhan42-crypto/victory-rtx20-ultimate.git
cd victory-rtx20-ultimate

# Optimized release binary
cargo build --locked --release -p victory --bins

# Release build with optional diagnostics hooks
cargo build --locked --release -p victory --features diagnostics --bins
```

---

## Running with Your Own Copy

Copy [victory.example.toml](victory.example.toml) to `victory.toml` (git-ignored; keep game paths outside the repository) and launch using [scripts/play-rtx20.ps1](scripts/play-rtx20.ps1) or [Play-VIctory.cmd](Play-VIctory.cmd):

```powershell
# Series S profile (recommended default for 6 GB GPUs)
.\scripts\play-rtx20.ps1 -GamePath "D:\Games\MyDump\game_scarlett_master.exe"

# Optional fullscreen / Series X / reduced DirectStorage RAM staging
.\scripts\play-rtx20.ps1 -GamePath "D:\Games\MyDump\game_scarlett_master.exe" -Fullscreen -DirectStorageHalf
```

Translated shaders are cached automatically under the configured `VICTORY_CACHE_DIR` and keyed by the BLAKE3 hash of the translator source and host capabilities, so updated builds re-translate modified shaders automatically.

---

## Documentation

- [docs/architecture.md](docs/architecture.md) — Core runtime, loader, and Direct3D 12 layer architecture
- [docs/asset-policy.md](docs/asset-policy.md) — Strict no-game-asset repository policy
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md) — Ray-tracing builder and `VICTORY_AMBIENT` behavior
- [docs/replay.md](docs/replay.md) — Frame capture and offline replay reference

---

## Credits

Built on upstream community work, preserving commit authorship and upstream attribution:

- **[VIctory](https://github.com/Marvisak/VIctory)** — Original loader, HLE, D3D12 backend, and RDNA 2-to-DXIL compiler
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** — Base fork and community pull requests
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**, **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**, **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**, **[myokex/victory-amd](https://github.com/myokex/victory-amd)**, **Geckomacabre**, **cxy0072019-sketch**, and upstream CGBJ/Wave32 contributors (commit history and attribution preserved)

---

## License

Licensed under **GPL-3.0-or-later** ([LICENSE](LICENSE)), subject to the single format-table exception documented in [docs/asset-policy.md](docs/asset-policy.md).
