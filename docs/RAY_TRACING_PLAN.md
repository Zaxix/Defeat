# Ray-traced lighting: where it stands and the plan (2026-11-27)

The game is dark on our PC mostly because its ray-traced light does not run.
This page says why, what is already in place, and the order to get it
working on the owner's PC (Ryzen 7 5800X, RTX 4070 12 GB, 64 GB). It is the
implementation plan; `docs/fork-ray-tracing.md` describes the machinery and
`docs/RESEARCH_CHECKLIST.md` (R14, R42, R50, R52, R53, R57) holds the research.

## What the game ray traces

On the base consoles the game ray traces its **diffuse global illumination**
(bounce light, the irradiance cache, screen probes) and **specular
reflections**. Direct sunlight and the main shadows are classic shadow maps.
So with ray tracing off, everything only bounce light reaches is black or
near black: interiors, shaded streets, the dark side of faces. That is the
picture of runs 3-12.

The game does not use DXR. Its ray tracing is its own compute shaders:

1. **The structure builder** (`XboxBvhBuilder.dll`, 52 compute programs:
   rehydration of the static world's shipped structures, triangle and
   instance loading, Morton codes, radix and bitonic sorts, BVH4
   construction, refit, treelets). It writes structures in the console's
   own node format into plain title memory, through raw 40-bit addresses.
2. **The walkers**: 64 programs (irradiance cache, screen probes,
   reflections, the shadow-ray family) walk those structures with a stack in
   shared memory and one hardware instruction per node,
   `image_bvh(64)_intersect_ray`, which we emulate exactly in DXIL
   (`victory-shader/src/bvh.rs`, `emit/intersect.rs`; checked bit for bit on
   the GPU against a CPU reference).
3. **Resolve and shading** (`CS_RaytracingResolve` and others) shade what
   the rays hit.

## Where it stands in this tree

| Piece | State |
|---|---|
| Builder programs translate | all 52 (the base, 2026-11-27 03:45) |
| Builder runs | only "whole": every builder pipeline needs a host pipeline (`pipeline.rs::Builder`), or none runs (a partly run builder once wrote garbage bounds into title memory and crashed the game) |
| Walkers translate | 59 of 64 before; the 5 shadow-ray programs (`CS_TraceShadowRays`) now translate (4 of 5 expected; the "Full" one also needs 64-lane waves) |
| Node test | emulated; with the builder off (`VICTORY_BVH_BUILDER=0`, our runs) every test answers "no hit", so nothing reads garbage |
| Memory by raw address | `memory.rs`: a directory per 64 KiB page leads shaders to the host copies; pages reached only through pointers read 0 the first frame |
| **Builder on** | **the game ends itself 30-40 s after the builder starts** (two base runs, with and without quarantine). That is its hang watchdog: the main thread stood still for 30 s |
| Cost with the builder on | not measured here; another tree of the same lineage reported 0.2-3 fps with a different memory design |

## Why the watchdog fires (what we know)

Checked against the owner's executable (1.0.2.1, BLAKE3 `b99f876b...`): a
thread polls once a second and, when the main thread's heartbeat (a QPC
value at image+0x71b6338) has not changed for 30 polls, or a timed scope is
older than 300 s, calls a handler that writes to address 0
(`victory_game::watchdog`). So with the builder on, the main thread waits
30 s for something. Candidates, in order of likelihood:

1. **A result the title waits for never arrives**: a builder store to a page
   that had no host copy is dropped (`memory.rs`, "lost access"), so a
   postbuild size, counter or fence-like value the title reads back stays
   0, and a CPU wait never ends.
2. **A frame of more than 30 s**: the first builder dispatch makes all
   builder pipelines on the spot, and every list of builder work brings
   every numbered copy up to date and declares it written.
3. **A real deadlock** between the title's threads and ours.

## The plan

### Step 1: see it (in this tree now)

- **Stall watch** (on by default): once the main thread has not moved for
  10 s, every 10 s the log says what our side was doing: the title's fence
  waits outstanding (fence, value, how long), the builder's state, the
  submitting thread and the trail of recent work.
- **`VICTORY_NO_WATCHDOG=1`** (off by default): the watchdog logs instead
  of ending the run. Byte-checked; this build only.
- **Memory-by-address line** every frame-rate line: draws and dispatches
  that use it, copies brought up to date and declared written (count, MB).
- **Builder switch no longer rebuilds the whole shader cache**: only the
  ~100 programs that test rays are keyed on it.

### Step 2: a builder test run (run 14, after run 13)

Builder on, walkers held until it has run whole, the watchdog logging,
crash tracing on, the resolve programs that lost the base's card held back:

```powershell
$env:VICTORY_BVH_HOLD = "1"
$env:VICTORY_NO_WATCHDOG = "1"
$env:VICTORY_D3D12_DRED = "faults"
$env:VICTORY_DENY_PIPELINES_FILE = "C:\VIctory\diagnostics\deny-raytracing-resolve.txt"
# and NO VICTORY_BVH_BUILDER line (unset = on)
```

What it answers: does the builder run whole on the RTX 4070 ("the
ray-tracing structure builder runs" line); what the stall lines say if the
main thread stops; what the memory line costs; whether bounce light appears.

### Step 3: fix what step 2 shows

- If a **result never arrives**: make every page of the title allocation a
  builder dispatch names ready before it runs (the extent of the
  allocation, which the memory module of the layer knows: `memory.rs`'s
  first shortcut), so no builder store is lost.
- If **frames are too long**: mark pages written on the GPU. Each raw store
  sets a bit for its 64 KiB page; after the submission only marked pages
  go back, instead of every numbered copy (upstream VIctory's `memory.rs`
  has this design: a per-list dirty bitmap, `copy_back_page`). This is the
  big one for speed with ray tracing on.
- If the **card is lost**: DRED's page fault address and the trail say
  which program; it goes on the deny list while it is fixed.

### Step 4: make it right, then fast

- Shadow-ray "Full" (64-lane, `07c93bfb77fecebb`): the paired form, as the
  water programs; until then it stays held.
- Resolve programs back in, one family at a time.
- Water and the 26 other 64-lane programs: the paired form per hash, each
  checked against the CPU reference before it is on by default.
- Only then performance of the traced passes themselves (the card).

### What we will not do

- **Host DXR in place of the title's own walkers.** The walkers are the
  game's code in 64 programs; replacing them means recognising each loop
  and rebuilding the scene's geometry for the host. XCL's design issue
  (#1751) is the reference if it is ever tried; nobody has it working.
- **Approximate GI** (a constant ambient term in place of bounce light):
  it would look less black and be wrong everywhere.
- **The watchdog bypass as a fix.** It is for finding out what the long
  wait is.
