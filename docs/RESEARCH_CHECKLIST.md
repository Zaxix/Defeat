# VIctory Research Checklist — NVIDIA RTX 4070 / Ryzen 7 5800X

> **Research-only tracking file.** This is separate from Claude's implementation work. Keep existing Claude notes, tasks, commits, and branches untouched. Research entries are **not** instructions to merge, cherry-pick, or change source automatically. Only mark an item complete when its stated evidence has been recorded.
>
> **Target PC:** RTX 4070 12 GB, Ryzen 7 5800X, 64 GB DDR4-3600. **Deferred by owner:** adaptive memory manager / automatic memory-placement policy — do not implement or reopen until requested.
>
> **Snapshot (2026-11-27):** default branch `main` at `fc093e2ca81095d9c090fdfe99b79353030c5984`. Experimental branches include `exact-lock`, `loose-lock`, `shader-licm`, `texture-digest`, `gpu-access-costs`, `gpu-access-flags`, `merge-words`, `dstorage-before-write`, and `gta6`. Branches **diverge** from main; a commit's presence on an experiment branch does **not** mean it is deployed on main. Re-check ancestry and latest head before each investigation.

## Working agreement / evidence gate

- [x] **R00 — Confirm access and inventory.** Repository, README, architecture, and branch names inspected (2026-11-27); target PC recorded.
- [ ] For every proposed fix, record: repository + branch + commit SHA; observed issue and exact reproduction; profiling data; relevant tests; Windows/NVIDIA build result; side-by-side visual correctness; whether change is already present on target branch.
- [ ] Maintain three statuses separately: **researched**, **implemented on a branch**, **verified on the owner's PC**. Do not equate code review or a Linux/cfg(windows) type-check with a Windows build or working in-game.
- [ ] Use one reproducible world scene / camera / console profile / resolution / driver / warm shader cache across comparisons. Log median and p95 frame time, CPU prepare, GPU execution, copy-back, upload bytes, memory/VRAM budgets, and crashes.
- [ ] Avoid speculative merges, broad cherry-picks, and combining competing changes without an ancestry + conflict + correctness audit. Claude owns all source changes and PRs.

## P0 — Establish a baseline and identify the critical path

- [ ] **R01 — Capture the RTX 4070 / 5800X baseline on the actual branch to be tested.** Record current SHA, NVIDIA driver, Windows version, firmware/Resizable BAR state (observed, not assumed), Series X vs S mode, game build, resolution, and shader-cache warmth. Measure at least three consistent runs. **Done when:** reproducible baseline frame-time table + settings + logs exist.
- [ ] **R02 — Separate CPU time from GPU and synchronization time.** Trace per-frame command preparation, list worker waits, texture hashing, detiling, actual upload, copy-back, fence waits, and draw/dispatch work. `VICTORY_GPU_TIMING=1` and GPU-based validation alter timing and must *not* be treated as normal release FPS measurements. **Done when:** critical-path timeline identifies the top two measured costs. Source: [README debugging flags](../README.md).
- [ ] **R03 — Compare main with each experiment branch by commit ancestry / scoped diff.** Record which experimental fixes are absent from main, which overlap, and which have been superseded. **Done when:** a branch-by-branch table shows `SHA → files → tests → result → target status`. Do not assume a merged upstream fix is absent until source checked.

## P1 — CPU-side submission, resource creation and locks

- [ ] **R04 — Investigate `shadow::EXACT` lock contention and buffer lifecycle.** Experimental commit [`16183bd7`](https://github.com/Zaxix/VIctory/commit/16183bd7c578f175beab7c076d6c5eba0346fedd) moves allocation/release outside the lock and throttles prune scans, but **does not** port fence-safe buffer recycling from Belobug PR #451. Profile waits, resource creation rate, duplicate-on-race resolution, outstanding GPU references, and release timing. **Done when:** cold/warm scene performance + fence lifetime tests pass on Windows/NVIDIA. Research: [Belobug PR #451](https://github.com/Belobug/VIctory-Belobug-AMD/pull/451).
- [ ] **R05 — Investigate `shadow::LOOSE` lock contention and overlapping windows.** Experimental commit [`dc744dbb`](https://github.com/Zaxix/VIctory/commit/dc744dbb8d5a4013176c31b324d7801e9a99cc80) changes the mutex to an `RwLock` and builds missed windows outside it. Verify overlap semantics, cleanup and concurrent lookup/insert; measure on Ryzen 5800X. **Done when:** p95 lock wait and resulting frame-time delta are reported. Reference: [Belobug PR #443](https://github.com/Belobug/VIctory-Belobug-AMD/pull/443).
- [ ] **R06 — Command-list submission and indirect dispatch.** Inspect command signature selection, transient buffer allocations, per-list setup, residency waits, list reuse and worker utilization. Evaluate fxceless's lean indirect dispatch independently of memory changes. **Done when:** GPU vs CPU timings both logged; no missing indirect work. Reference: [fxceless PR #1352](https://github.com/fxceless/victory-gta6/pull/1352) (verify repository/PR availability).

## P1 — Texture hashing, detiling and streaming

- [ ] **R07 — Measure unchanged-texture checks separately from transfers.** Experimental [`63b87e8`](https://github.com/Zaxix/VIctory/commit/63b87e8ca5e26f9c20194ea576fd7d93281a25f3) uses XXH3-128 with `VICTORY_TEXTURE_DIGEST=blake3` comparison and logs bytes checked vs unchanged. Check changed bytes, remap/unmap races, CPU cost, and content correctness. **Done when:** controlled blake3 vs XXH3 measurements and race regression tests are recorded. References: [upstream issue #916](https://github.com/Marvisak/VIctory/issues/916), [#3269](https://github.com/Marvisak/VIctory/issues/3269).
- [ ] **R08 — Profile detiling, texture invalidation and actual CPU→GPU uploads.** Do not count bytes read for digesting as upload bandwidth. Determine whether partial-tile refresh or content-aware invalidation saves the measured critical path, without skipping legitimate writes. **Done when:** hashed/detiled/uploaded byte and ms counters reconcile, screenshots match baseline. References: [#915](https://github.com/Marvisak/VIctory/issues/915), [#916](https://github.com/Marvisak/VIctory/issues/916).
- [ ] **R09 — DirectStorage/memory-write ordering.** Review the experimental `dstorage-before-write` and `merge-words` branches, ensure writes invalidate/refresh mirrored data in correct order. **Done when:** deterministic storage-write/readback regression scenarios pass; no stale texture or geometry data. Reference: [upstream issue #2503](https://github.com/Marvisak/VIctory/issues/2503).

## P1 — NVIDIA GPU correctness and shader execution

- [ ] **R10 — Validate capped NVIDIA shader-visible descriptor heaps.** Main includes [`fc093e2`](https://github.com/Zaxix/VIctory/commit/fc093e2ca81095d9c090fdfe99b79353030c5984), restoring usable space for host-copy view pairs. Confirm the actual descriptor budget, dynamic slot exhaustion, remap behavior, null views, and GPU validation on the RTX 4070; avoid duplicating this fix. **Done when:** no descriptor exhaustion, out-of-range accesses, or silently dropped view updates over a long world run.
- [ ] **R11 — Evaluate DXIL LICM and barrier elimination from the experiment branch.** [`2293f308`](https://github.com/Zaxix/VIctory/commit/2293f3082162fb492558784896bf3cb24688d9d0) ports optimization passes (passes 256/512 in this tree). Run shader tests, DXIL validation and controlled GPU-time comparisons. Protect IEEE/NaN, memory ordering, wave64-on-wave32 semantics, water and geometry correctness. **Done when:** shader hashes, validation rate, GPU time and side-by-side reference captures show no new failures. Source: [klbf-lab PR #41](https://github.com/PastiiJay/klbf-lab/pull/41).
- [ ] **R12 — Identify truly slow translated shaders.** Correlate logged hash + guest pipeline + descriptor inputs + dispatch dimensions, then compare generated DXIL and console instructions. Evaluate specific optimizations *only after* confirming where GPU time is spent; never mask a bad buffer input by blindly shortening loops. **Done when:** a top-N shader-cost table and per-shader correctness tests exist.

## P2 — GPU synchronization and rendering defects

- [ ] **R13 — Fence-safe recycling and copy-back concurrency.** Cross-check [PR #2112](https://github.com/Marvisak/VIctory/pull/2112), [PR #3102](https://github.com/Marvisak/VIctory/pull/3102), and Belobug [#451](https://github.com/Belobug/VIctory-Belobug-AMD/pull/451). Trace CPU writes during GPU readback, queue dependency ordering, release/reuse only after fence completion, and device removal. **Done when:** targeted race tests survive parallel CPU writes, repeated remaps and asynchronous GPU completion. *Scope excludes building a new adaptive memory-placement policy.*
- [ ] **R14 — Ray-traced shadows / `XboxBvhBuilder` → host DXR.** Determine what builder and rehydration calls are actually used, whether acceleration structures are constructed and barriers correct, and why shadows stay black. Do not confuse ambient-light approximation with working ray-traced shadows. **Done when:** trace + host validation + matching game capture confirm working structures, or the exact missing operation is isolated.
- [ ] **R15 — Wave64/format/occlusion correctness.** Verify NVIDIA wave32 fallback, unsupported typed UAV format handling, occlusion query semantics and depth bounds. Main already contains a [null-view crash fix](https://github.com/Zaxix/VIctory/commit/219878a0e7754511d89306331812971bb2153f5f) and [typed-UAV guard](https://github.com/Zaxix/VIctory/commit/ea488a9f9901e4375c547a57795ae8cb79d577a4); avoid reimplementing. **Done when:** captured output, draw counts, validator logs and regression cases match expected behaviour.

## P2 — Long-run reliability and optional rendering profile

- [ ] **R16 — Long-duration NVIDIA stability.** Repeat 30–60+ minute runs with DRED enabled, record GPU resets, live resource and descriptor counts, residency changes, fence waits and cumulative memory. Compare short clean run vs late-session state. **Done when:** a specific failure site or stable non-regressing run is documented.
- [ ] **R17 — Series S mode on NVIDIA as a separately labelled experiment.** Confirm the actual render profile, supported output/UAV formats and upscaler path. Report Series X vs S frame times only under otherwise matched scene/settings; do not compare RX 7900 XTX Series S scores with RTX 4070 Series X results as equivalent. **Done when:** paired correctness/FPS measurements exist.

## P2 — Scarlett executable cross-reference (no runtime claims)

- [ ] **R18 — Relate five guest memory regions to GPU usage.** Use the already reported `0x01E86440` function RVA as a **build-specific analysis lead only**. Verify it against the exact Scarlett executable checksum, reconstruct caller-controlled region sizes, map source/destination permissions and all descriptor creations. Determine whether any of the three conjectured read-only pools receives a writable GPU UAV **with proof**, not assumptions. **Done when:** per-region allocation/writer/descriptor evidence table exists.
- [ ] **R19 — Connect guest GPU calls to VIctory performance events.** Correlate reported `XboxBvhBuilder`, texture streaming and `XMemTransferMemory` call chains with actual emulator trace/queue timing, not raw disassembly alone. **Done when:** a guest operation → host command → time/correctness link is reproducible.

## Deferred / explicitly not in scope

- [ ] **PARKED — Adaptive memory manager / automatic VRAM placement.** User explicitly postponed this. Do not start it without further instruction.
- [ ] No claims of 22 FPS, stable rendering, or completed Windows builds without running the user's actual build on the user's hardware under matched settings.

## Source pointers and ownership

- This fork: https://github.com/Zaxix/VIctory
- Upstream: https://github.com/Marvisak/VIctory
- Relevant contributor project: https://github.com/Belobug/VIctory-Belobug-AMD
- README and platform architecture: [README](../README.md), [architecture](architecture.md).
- **Claude:** implements, tests, commits, handles pull requests and integration.
- **Research assistant:** records evidence, cross-references commits, maintains **this research checklist only** when permitted, never removes Claude's content.
