# cull — requirements

A fast, GPU-based image culling tool for Linux. Purpose: review large shoots of paired
JPG+ARW files and delete the rejects quickly. It is **not** a photo manager, editor, or DAM.

Status legend: `TODO` / `WIP` / `DONE` / `BLOCKED` / `DROPPED`

---

## R1 — Instant navigation (the defining requirement)

> Very very fast rendering of images and switching between adjacent 5-10 images in either
> direction must be completely instantaneous.

**Status: DONE and measured** — see the verification table below.

- Switching to an already-prefetched image must be a texture bind + matrix update. Target
  well under one frame (< 5 ms), i.e. perceptually instant, not merely "fast".
- Prefetch window of ±10 groups around the cursor, kept warm at all times.
- Navigation must never block on decode. If a slot is cold, show the previous frame or a
  low-res placeholder rather than stalling.
- Bursts matter more than steady state: holding or hammering the next-image key must not
  outrun the prefetcher into a visible stall.

Hardware target: RTX 3090 (24 GB VRAM), high-core-count CPU.

### Measured, and the decision it forces

Benchmarked on the real set (`cargo run --release --bin bench`). Sandbox was limited to
**2 rayon threads**, so parallel figures are a floor, not a prediction for the target box.
Serial figures are hardware-independent enough to design against.

| path | serial | median/img |
|------|--------|-----------|
| zune-jpeg full-res → RGBA | 145 MP/s | 172 ms |
| jpeg-decoder full-res | 103 MP/s | 246 ms |
| jpeg-decoder DCT-scaled to fit 4K | 103 MP/s | 239 ms |
| jpeg-decoder 1/4 scale | — | 128 ms |
| **ARW embedded preview → zune** | **228 MP/s** | **109 ms** |

Three conclusions, all of which simplify the design:

1. **Decode at full resolution. Do not use DCT scaling.** For 6192×4128 fit to a 4K
   viewport, the needed size is 3240×2160; half-scale is 3096×2064, which *undershoots*,
   so the scaler legitimately declines to engage and buys nothing. Even forcing 1/4 scale
   only recovers ~48% of the time, because Huffman entropy decoding dominates and DCT
   scaling does not reduce it. Full-res decode is therefore both faster in practice and
   better quality.
2. **This removes the reload-on-zoom requirement from R5 entirely.** Textures are always
   native resolution, so zooming to 100% and beyond needs no re-decode. R5 gets simpler.
3. **ARW is the *cheaper* source, not the more expensive one** — its embedded preview is
   5.0 MB against the sidecar JPEG's 11.6 MB, so it decodes in ~63% of the time at
   identical 6192×4128 resolution. RAW support is a performance win, not a cost.

Decoder choice: **zune-jpeg**, emitting RGBA8 directly (no widening pass before upload).
`jpeg-decoder` is retained only as the DCT-scaling comparison in `bench`.

VRAM: 102 MB per image at full res; a ±10 ring is 21 images ≈ **2.1 GB**. Comfortable
against 24 GB, and leaves room to widen the ring if wanted.

Cold-slot latency is ~172 ms serial. On a 16-thread machine the refill rate should be
roughly 40–90 img/s, far above any human key-repeat rate, so the ring stays warm and the
prefetcher does not need to be clever. Sustained navigation is the easy case; only a cold
start or a random jump can stall.

### End-to-end verification (`bin/ringtest`, real files, radius 10)

Measured on **2 cores** — the sandbox limit, roughly 8× less than the target machine — so
these are pessimistic by a wide margin.

| demanded rate | already warm on arrival | median | worst |
|---------------|------------------------|--------|-------|
| 5 img/s | **40/40** | 0.00 ms | 0.02 ms |
| 6.7 img/s | **30/30** | 0.00 ms | 0.02 ms |
| 10 img/s | **30/30** | 0.00 ms | 0.00 ms |
| 16.7 img/s | 14/30 | 3.98 ms | 91 ms |
| unlimited | 4/40 | 89 ms | 187 ms |

**R1 is met.** Below the decoder's production rate every image is already resident and
arrival costs ~0 ms. The knee sits between 10 and 16.7 img/s, matching the predicted
2-thread rate of 11.6 img/s (172 ms ÷ 2) almost exactly — so the model is sound and the
knee should scale linearly with cores, landing near **90 img/s on 16 threads**.

Two things worth noting:

- The "unlimited" row is **producer-limited, not a ring defect**: consuming with zero
  think time means always waiting on the decoder, and its 89 ms median is precisely the
  2-thread per-image rate. It is a measure of decode throughput, not of prefetch.
- Past the knee, degradation is **graceful** — median 4 ms, worst 91 ms, no stalls. There
  is no cliff.

Cold start (nothing prefetched) is ~190 ms and unavoidable; it is one image, once.

## R2 — Linux only

**Status: DONE**

No Windows/macOS support required. Wayland and X11 both via winit. This licenses us to use
Linux-only paths (freedesktop trash, colord) without abstraction layers.

## R3 — Directory tree sidebar with grouped entries

> Tree-preview of directories on the side, with being able to select images there
> (JPG+ARW) grouped, no need to render ARW

**Status: DONE** (`src/scan.rs`, `src/ui.rs`)

- Collapsible directory tree, click to select an image.
- One row per **group**, not per file. `A6709605.JPG` + `A6709605.ARW` is one entry.
- Grouping key is the basename stem, taken as everything before the **first** dot. That
  rule is what catches RawTherapee's `A6701113.ARW.pp3` and `A6700134_Export.jpg.out.pp3`,
  which a JPEG↔RAW pairing rule would miss.
- Display candidate preference: `jpg` → `jpeg` → `arw`. RAW is used only as a fallback.
- Groups with no displayable member (an orphaned `.pp3`) are dropped from navigation.
- Symlinked directories are not followed, so link cycles cannot hang the scan.

### Validated against the real tree

`cargo run --release --bin scanrep` over 12 directories: **4287 files → 2109 groups**,
with every file accounted for and zero groups lacking a displayable member.

The scan itself does no I/O beyond `read_dir` — file sizes and timestamps are gathered
separately and off the launch path, because stat'ing every file is what made a cold start
take seconds. See R23.

| shape | count | meaning |
|-------|-------|---------|
| `arw+jpg` | 2040 | the ordinary case |
| `arw+jpg+pp3` | 35 | one RawTherapee sidecar |
| `arw+jpg+pp3+pp3` | 17 | sidecars for both the ARW and the JPG |
| `jpg+pp3` | 17 | an export plus its `.jpg.out.pp3` |

No raw-only groups exist in this set, but the path is implemented and tested.

### Export files — confirmed by the user

`A6701135_Export.jpg` has a **different stem** from `A6701135.JPG`, so deleting the
original does **not** delete the export, and the export appears as its own navigable
entry. **User confirmed this is the wanted behaviour.** Exports are derived work to be
preserved and viewed independently.

## R4 — Deletion

**Status: DONE** (`src/trash.rs`, `src/app.rs`)

- Deletes the **entire group** (JPG + ARW + all `.pp3` / `.xmp` siblings).
- Moves to freedesktop trash. **Never** `rm`. Culling is fast and misfires are expected.
- Undo stack to restore recent deletions (see R8).

## R5 — Mouse zoom and pan

**Status: DONE** (`src/view.rs`, `src/main.rs`)

- Mouse wheel zooms, anchored at the cursor position (not at image centre).
- Click-drag pans.
- ~~Zooming beyond the decoded scale triggers a full-resolution reload.~~ **Dropped**:
  R1's benchmark showed full-res decode is the fastest path anyway, so textures are always
  native resolution and no reload is ever needed. See R1.

## R6 — Two fit modes

> Centering of the image so that it fills the screen (with X, how it is in geeqie), i.e. two
> modes: one where on image switch it recenters, and one which keeps the zoom level

**Status: DONE** (`src/view.rs`, toggle bound to `X`)

- **Refit mode**: every image switch resets to fit-to-window, centred.
- **Preserve mode**: zoom factor and pan offset carry across switches — for comparing the
  same region across adjacent frames (focus checking).
- Toggle bound to a key. Geeqie's equivalent is bound to `X`; match that unless it clashes.

## R7 — EXIF orientation

**Status: DONE** (`src/view.rs`, applied as a UV transform in the shader)

Must be honoured or portrait shots display sideways. Real data contains a mix of
orientation 1 and 8. Applied as part of the render transform, not by rotating pixels.

## R8 — Trash safety and undo

**Status: DONE** (`src/trash.rs`, 64-step undo)

- Trash, not delete (see R4).
- Undo stack restoring the last N deletions from trash.
- Highest value-per-line feature in the project. Do not skip.

## R9 — RAW support via embedded preview

**Status: DONE** — confirmed cheap, so in scope. Turned out to be *faster* than JPEG.

Sony ARW files embed **three** JPEGs. Verified on `A6709605.ARW` (31.8 MB):

| offset  | size        | encoding | bytes  |
|---------|-------------|----------|--------|
| 43898   | 160×120     | baseline | 0.01 MB |
| 192674  | 1616×1080   | baseline | 0.46 MB |
| 655360  | **6192×4128** | baseline | 5.00 MB |

A full-resolution preview is present, so **no demosaicing is needed** and RAW display costs
about the same as a sidecar JPEG. Extract via TIFF/EXIF IFD tags (robust) rather than
scanning for SOI markers (fallback only).

Consequence: RAW-only shots work fine. Missing-JPEG is not an error case.

## R10 — ICC colour management

> Do ICC then, color management is important, the images should look like how they look in
> other viewers

**Status: DONE by analysis — no colour-conversion code will be written.**

The requirement is satisfied by doing nothing, and that conclusion is forced by the data.

### Measured

| source | profile | verdict |
|--------|---------|---------|
| Camera JPEGs (2184 files) | none embedded; EXIF `ColorSpace = sRGB` | sRGB |
| RawTherapee exports (16 `.jpg`) | 748 B, `desc = RTv4_sRGB`, parametric TRC | sRGB |

Every export profile is byte-identical and its primaries are textbook sRGB/Rec.709
(`rXYZ = 0.4360, 0.2225, 0.0139`, D50 PCS white point `0.9642, 1.0, 0.8249`). Checked with
`/tmp/icc.py` across all 16 profile-bearing files — zero variation.

The user's display has **no colour profile configured** (confirmed directly), so the display
side is sRGB by assumption.

### Decision

sRGB → sRGB is the identity transform. Building lcms2 + a 3D LUT would add a dependency, a
config surface, and a shader stage to produce **no pixel change** on the entire library.
Not doing it. This is a deliberate rejection of the original R10 design, on evidence.

### What is actually implemented instead

The one colour decision that genuinely changes appearance:

- Textures use **`Rgba8UnormSrgb`** on an sRGB surface, so the GPU converts sRGB→linear
  before filtering and linear→sRGB on output, in fixed-function hardware.
- This matters because JPEG samples are sRGB-encoded and therefore non-linear. Filtering
  them as if linear — the common mistake — makes downscaled detail come out subtly dark.
  Getting this right is what makes output match other viewers; ICC plumbing would not have.

### Guard against silent wrongness

A file whose embedded profile is *not* sRGB-like (primaries outside a small tolerance)
must be **flagged in the UI**, not silently rendered wrong. Detection only, no transform.
If that ever fires on real data, building the transform is a contained follow-up and this
section should be revisited.

The guard was **unreachable** until fixed: `note_shown` was called with `icc_profile:
None` from both of its call sites, so `icc::classify` never saw a profile and could only
ever return `AssumedSrgb`. The verdict is now computed at upload time, where the decoded
`Image` still carries its profile, stored on the texture entry and reported through
`App::note_colour`. It is deliberately separate from `note_shown`, because the view is
laid out from the file header before decoding (R14) and re-running `note_shown` later
would refit the view and discard the user's zoom.

Deliberately **not** implemented: colord/D-Bus lookup, `_ICC_PROFILE` X atom, display
profile config, rendering intents, black point compensation.

## R11 — Tolerate a tree that changes underneath us

**Status: DONE** — added after the user confirmed they edit the library concurrently.

The file tree is **not stable**. Files appear and disappear while the tool is running,
both from the user's own culling elsewhere and from our own deletions. This is a stated
fact about the workflow, not an edge case, so it is a first-class requirement.

- A path that vanishes between scan and decode must produce a placeholder and a log line,
  never a crash and never a stall.
- The prefetch ring must tolerate holding paths that no longer exist.
- Deleting a file that is already gone is a **success**, not an error.
- A failed slot must be remembered as failed, not retried in a tight loop.
- Counts and indices from a previous scan may be stale; never assert on them.

Deliberately **not** implemented: inotify/auto-refresh. A manual rescan is enough, and
watching 85 GB across 12 directories is complexity we have no demonstrated need for.

## R12 — Preload on hover

**Status: DONE** (`Prefetcher::hint`, `src/ui.rs`)

> When hovering over an image with the mouse you should already preload it immediately,
> as the user will likely click on it.

Hovering a sidebar row calls `Prefetcher::hint(index)`, which queues that index at
distance 0 — ahead of the whole window — without moving the window itself. **Exactly one
hint is retained**, so sweeping the pointer down a long list cannot queue unbounded work
or grow memory. The hint slot is exempt from window eviction until replaced or cleared.

## R13 — Asynchronous UI

**Status: DONE**

> The ui seems to be quite laggy when loading an image, it should all be asynchronous.

Three separate causes, all fixed:

1. **The event loop was polling.** `ControlFlow::Poll` plus a `pump_uploads()` in
   `about_to_wait` meant the main thread spun a core and took the ring's mutex on every
   iteration — starving the very decode workers it was waiting for. Now
   `ControlFlow::Wait`, and the ring wakes the loop through an `EventLoopProxy` the
   moment a decode completes (`Prefetcher::set_waker`). No polling at all.
2. **Unbounded uploads per frame.** `pump_uploads` uploaded every ready image at once.
   After a jump that is up to 21 × 102 MB ≈ **2.1 GB of staging copies in a single
   frame** — a guaranteed stall. Now capped at 2 per frame, nearest-to-cursor first, with
   the rest returned via `Prefetcher::put_back` and drained over following frames.
   The image being looked at is always exempt from the cap.
3. **Deferred uploads could stall.** Since the ring's wake had already fired, leftovers
   would have sat uncollected until the next input. `pump_uploads` now reports
   outstanding work and the frame requests another redraw until the queue drains.

Decoding itself was already off-thread; the lag was entirely in how the main thread
collected the results.

## R14 — No visible resize when an image loads

**Status: DONE** (`decode::probe`, `App::apply_probed_size`)

> It keeps resizing after loading an image, which is jarring. It should immediately load
> the image in the correct size.

The view used to be laid out only once the texture arrived; before that `shown` was
`None`, so it fitted a 1x1 placeholder and then refitted when the real image landed —
reading as the window resizing itself.

Now selecting an index memory-maps the file and reads dimensions plus orientation from the
header alone (`decode::probe`, reusing `locate` and `jpeg_dimensions`). Only the few pages
holding the header and IFDs are touched, so it costs microseconds even for a 32 MB ARW,
and it is cached per index. Tested against the real library to agree **exactly** with a
full decode, and asserted to stay under 20 ms/image so it is safe on the UI thread.

The **first** image was missed: `apply_probed_size` ran on every *selection*, and nothing
selects index 0 at startup, so `shown` stayed `None` until the first texture landed and
image 0 alone still fitted a 1×1 placeholder and then jumped. `App::new` now probes it.

## R15 — Delete asks for confirmation

**Status: DONE**

`Delete` arms the delete and shows what will go (`stem`, file count, total size);
**Enter** carries it out, **Escape** cancels. Escape only quits when nothing is armed, so
it cannot both cancel and exit.

Originally `Shift+Delete`. The modifier was dropped at the user's request: arming is not
itself destructive, and the deliberate second keystroke is **Enter**, which is where the
safety actually lives. Trash-not-`rm` and the 64-step undo are unchanged, so the guard is
no weaker for it.

Navigating also disarms it, because otherwise an Enter intended for something else could
delete a group the user was no longer looking at. Confirmation only ever applies to the
group that was armed, checked against the current index.

## R16 — Hover preload is debounced

**Status: DONE** (60 ms, `Shell::update_hover`)

Hinting on every frame of pointer movement queued work for every row swept over. The
pointer must now rest on a row for **60 ms** before it is preloaded, and the clock
restarts whenever the row under the pointer changes.

## R17 — Manual zoom leaves refit mode

**Status: DONE** (`App::take_manual_control`)

> When zooming it should immediately switch from mode: refit, so that then pressing x
> recenters immediately, without having to press twice.

Zooming, panning or asking for 1:1 now switches the mode to *keep zoom*. Previously the
mode still claimed "refit" while the view plainly was not fitted, so the first `X` only
moved to preserve (no visible effect) and a second was needed to refit. One press now
recentres.

`Z` (1:1) is included deliberately: without it, navigating away would silently discard the
1:1 the user just asked for.

## R18 — Held navigation keys must not lock the UI

**Status: DONE** (`App::step`, `App::tick`, `Prefetcher::set_radius`)

> Scrolling through a lot of images by holding the arrow key also locks up the ui.

A held key produces navigations far faster than a 25 MP image can be decoded. Each one was
queueing a full ±10 window, so the decoders were kept permanently saturated with work that
was discarded before it could be displayed — starving the cores the UI needs.

Now navigations closer than **90 ms** apart are treated as a held key, and the window
shrinks to **radius 1** for the duration: only the image on screen and its immediate
neighbours are decoded. Once navigation has been quiet for **140 ms** the full radius is
restored around wherever the user landed. The event loop schedules that check with
`ControlFlow::WaitUntil`, so it still sleeps rather than polling.

### Throttling the decode target, not the selection

Shrinking the window was not enough. Key repeat can exceed 100 events/s while a 25 MP image
takes ~170 ms to decode, so re-pointing the decoders on every event guarantees that
**nothing ever finishes** — the view stays frozen for the whole hold.

So while a key is held, the **selection tracks every event** but the decode target only
moves every **250 ms** (about four images a second). Images therefore land periodically
during the scroll, which is what makes it feel like it is going somewhere. On settle the
target snaps to wherever the user actually stopped and the full window refills.

The view is deliberately *not* resized for images that are only passed through: doing so
would fit the view to an image that is not the one on screen.

## R21 — The tree must follow the selection

**Status: DONE**

> When descending with the arrow keys it often gets off screen with the current image in
> the tree.

Two problems. `scroll_to_me` cannot work with a virtualised list, because the selected row
is usually not instantiated. And forcing an absolute centred offset every time the
selection moved fought the user's own scrolling and mis-computed the target.

The list now carries its scroll offset between frames and applies the **smallest**
correction that brings the selected row back into view, with a two-row margin so the
selection never sits flush against the edge. When the row is already visible, the offset is
left alone.

### It did not work. Three separate bugs

Reported as "the selected image always goes off screen when scrolling", "at some point I
can't scroll further either", and "I can only scroll in the vicinity of the current
selected image". Three independent causes.

**1. The rows wrapped, so they were not a uniform height.** This is the one that made the
selection unfindable. A virtualised list is built entirely on every row being the same
height: `show_rows` maps scroll offset to row index by dividing by a fixed pitch. The row
text is four padded columns — 56 monospace characters, which egui lays out at **446 px**
— against ~364 px of usable width at the old 380 px sidebar. So every row wrapped to two
lines and was **32 px tall while `show_rows` had been told 18**. The offset-to-row mapping
was wrong by nearly 2×, and no amount of correct arithmetic elsewhere could compensate.
Rows are now truncated to a single line at any panel width, and `SIDEBAR_WIDTH` is 480 so
the columns actually fit by default — the old value was *intended* to fit them and never
did.

**2. The row pitch was computed wrongly.** `show_rows` takes a height *without* spacing
and adds `item_spacing.y` itself; it was handed one that already included the spacing.
That alone drifts a spacing per row — 1500 px by row 500. `row_metrics` now returns the
height and the pitch together from a single place so they cannot disagree, and floors the
height at `interact_size.y` (18), which is what egui actually uses — `text + 2 × padding`
is only 17.125.

**3. The correction was re-applied on every frame.** So the list snapped back the instant
the user scrolled anywhere else, and near the end of the list — where the two-row margin
asks to scroll past the content and gets clamped — it re-asserted the same offset forever
and pinned the scrollbar outright. The tree now follows the selection only on the frames
after it actually *moved*, and the target is clamped to the real end of travel. Clicking a
row deliberately does not scroll, since the user picked something already visible.

### Tested against egui, not against a second copy of the sums

Bug 1 is invisible to any test of the arithmetic alone — the sums were self-consistent and
still disagreed with what egui laid out. `tests/sidebar_scroll.rs` therefore drives the
real `ui::draw` through a real `egui::Context`: egui is pure CPU, so the sidebar lays out
here despite the sandbox having no display. It asserts the selected row's actual `Rect`
falls inside the list's actual viewport across 600 consecutive steps each way, across
jumps, and that wheel scrolling stays where the user put it. Wheel input is fed as real
events, because assigning the offset would prove nothing: egui keeps its own retained
scroll state and only moves for genuine input or an explicit override.

## R19 — Bounded per-frame upload cost

**Status: DONE**

> When clicking on an image which is out of cache it's very noticable that the ui locks up.

`queue.write_texture` for a 25.6 MP image is ~102 MB: a CPU memcpy into a staging buffer
plus the transfer, around **14 ms** — most of a 60 Hz frame. A fixed count per frame stalls
on slow hardware and wastes headroom on fast hardware, so uploads now run against a
**4 ms wall-clock budget** and the remainder waits for the next frame. The image actually
on screen is exempt, since it is what the user is waiting for.

### The exemption was documented but not implemented

The paragraph above described the intent; `pump` applied the budget to *every* pending
upload, the current image included. So a cold image finished its 14 ms of banding at
4 ms per vsync — **four frames of blank screen**, ~66 ms added to every cold arrival,
for no benefit at all: nothing is drawn until the texture is complete, so throttling it
protects nothing. `pump` now finishes `priority` in one frame and budgets only the
neighbours. Cold-image latency drops by roughly 50–65 ms.

Decode workers also now leave **two cores free** (`available_parallelism() - 2`).
Saturating every core is what made the window stop responding while a batch loaded.

## R20 — Virtualised sidebar

**Status: DONE**

An `egui::CollapsingHeader` body instantiates a widget for **every** child, visible or not,
so an expanded directory of 538 groups cost 538 widget layouts every frame. The tree is now
flattened into rows and drawn with `ScrollArea::show_rows`, which lays out only what is on
screen — roughly 40 rows instead of 538.

Consequences: collapse state is owned by the app rather than egui (the arrow glyph in each
directory row toggles it), and scrolling to the selection sets the offset directly, because
`scroll_to_me` cannot work on a row that was never instantiated.

## R22 — Do not read or allocate what will not be used

**Status: DONE** (`prefetch::FileLoader`, `decode::BufferPool`)

Two costs sat either side of the decoder, both invisible in a profile of the decoder
itself and both larger than they look.

### Reading whole files

`FileLoader` used `std::fs::read`. For an ARW that pulls **32 MB** off disk and into a
fresh heap buffer in order to decode the **5 MB** embedded preview (R9); the remaining
~27 MB of raw sensor data is read and then dropped untouched. The loader now maps the
file, exactly as `decode::probe` already did, so only the pages the decoder actually
touches are faulted in. Against an 85 GB library that will not sit in page cache, this is
the largest single I/O saving available, and it removes a full-size copy per image too.

The trade is that a file **truncated** while mapped faults rather than short-reading.
Deletion is safe (the inode survives the mapping), and R11 describes files appearing and
disappearing, not being truncated in place.

### Reallocating the output buffer

`zune`'s `decode()` does `vec![0; size]` per image. At 102 MB that is far above any
allocator's mmap threshold, so every image gets a fresh mapping and pays ~25k page faults
as the decoder writes it — and gives it all back on drop.

| | per image |
|---|---|
| fresh allocation, then written | **23.8 ms** |
| buffer reused, then written | **1.2 ms** |

~22.6 ms, about **13% of a 172 ms decode**, spent on nothing but page faults. Decoding now
goes through `decode_into` against a `BufferPool`; the renderer hands each buffer back
once its texture upload completes. Buffers are returned **at full length**, never
cleared — clearing would force a 102 MB memset to regrow and return most of the saving.

Best-effort by design: only the decode→upload→recycle path returns buffers. Images the
ring discards simply free theirs, which is correct, because a discarded image means the
user moved and the pool would only be holding memory for work nobody wants.

## R23 — Fast startup

> The image viewer takes a long time to start up. That should be really fast as well.

**Status: DONE** — launch is **~1 ms and independent of library size**. What remains
before a *photo* appears is GPU driver init plus the first decode; see "What is left".

> I want at least the application to open quicker than 100 ms, does not have to render an
> image immediately.

That distinction is what made this tractable: opening the window needs to know nothing
about the library at all.

`App::new` called `prefetch.set_centre(0)`, which queued the whole ±10 window — **11
full-resolution decodes** — before the window was created and before wgpu had an adapter.
With `available_parallelism() - 2` workers that is every core busy, and with a cold
`BufferPool` every one of them takes the fresh-allocation path (R22), so ~1.1 GB of
page-faulting ran concurrently with Vulkan driver initialisation and the first frame.

The neighbours cannot be navigated to before the window exists, so they are not needed
yet. `App::new` now queues **only index 0** (`Prefetcher::require`, which does not move
the window), and `App::warm_full_window` opens the ring to full radius after the first
frame has been presented.

Also: the instance is created with `Backends::VULKAN` rather than `Instance::default()`'s
probe-everything, so startup no longer spins up an EGL/GLX context purely to discard it.
`WGPU_BACKEND` still overrides.

### The ~3 s launch was one `stat` per file

Reported as "a long time to start when there are a lot of images, around 3 seconds", and
the fact that it **scaled with image count** is what identified it. The whole pre-GPU
startup measures **~10 ms** for 4320 files on a warm tmpfs, so nothing CPU-bound could
account for seconds. The only work proportional to file count was `group_files` calling
`std::fs::metadata` on every file: 4287 files at sub-millisecond each on a cold cache over
85 GB is almost exactly the three seconds reported.

Those stats fill two sidebar columns (size, capture time) and one confirmation message.
None of it is needed to show a photo. So:

- **`scan` no longer stats anything.** It is `read_dir` plus string work, proportional to
  directories rather than files.
- Sizes and times live in a `MetaStore`, filled by a background thread that starts *after*
  the first frame — deferred rather than merely moved, because thousands of stats are I/O
  bound and would otherwise queue against reading the first image off the same disk.
- Rows show `-` until it lands. Arming a delete stats that one group directly, so the
  confirmation always states what is going.
- Keyed by path, not index, so a deletion shifting every later index cannot attach stale
  sizes to the wrong rows.

### And then the scan itself came off the path

Even without the stats, the launch still waited for the whole tree to be walked. Reported
as: `./cull ~/Photos/` takes about a second, while one shoot folder is instant — i.e. the
cost tracked the size of the *root*, not of the shoot being viewed.

Opening a window requires knowing nothing about the library, so `main` no longer scans.
`App::empty` builds the app with no library at all and the window comes up immediately;
the scan runs on a worker and arrives as `Wake::Scanned`, handled by `App::adopt_scan`.

The prefetch radius is pinned to 0 until `warm_full_window` — so adopting a tree queues
exactly one decode, not twenty — and the app must tolerate every input while empty, which
it already did (R11 left it robust to a zero-length library).

Measured on synthetic libraries (2 rayon threads, tmpfs — floors, not predictions):

| phase | before | after |
|---|---|---|
| `scan`, 4320 files | 11 ms (incl. stats) | 4 ms, **off the launch path** |
| `scan`, 24 000 files | 17 ms (incl. stats) | 17 ms, **off the launch path** |
| stat pass, 24 000 files | on the launch path | 31 ms, **off it and parallel** |
| **launch, 200 files** | — | **1 ms** |
| **launch, 24 000 files** | — | **1 ms** |

Launch is now flat in library size, which is the property that actually matters: it cannot
regress as the library grows.

Trade-offs, both deliberate:

- The sidebar is empty and the status bar reads `scanning...` for as long as the walk
  takes. On a cold `~/Photos` that is visible.
- An empty or wrong directory no longer exits non-zero before the window opens; it opens a
  window reporting no images, and prints to stderr. A GUI that has already drawn cannot
  usefully `exit(1)`.

### What is left, and how to measure it

`RUST_LOG=info cull <dir>` prints:

```
startup: scanned N images in ...
startup: GPU adapter + device in ... (<adapter name>)
startup: render pipeline in ...
startup: stat'ed N files in ...
startup: first image on screen ... after launch
```

The window is up in ~1 ms, so anything remaining is GPU driver initialisation
(not controllable) or the ~172 ms full-resolution decode of the first image. Getting a
photo on screen inside 100 ms would need a **preview-first paint** — ARW carries a
1616×1080 preview that decodes in ~10 ms (R9) — with the full-resolution texture swapped
in behind it. Not built: it needs the renderer to keep showing the preview until the
replacement is complete, and that is a render-path change this sandbox cannot test.

---

## Explicit non-goals

Scope discipline is a requirement. The failure mode for this project is becoming another
geeqie. Not building: filmstrip, metadata panel, editing, tagging, ratings, slideshow,
collections, network/remote sources, thumbnail database, EXIF editor.

---

## Environment notes

- Test data: `/workspace/2026-08_Norwegen_Lofoten` — 2184 JPG+ARW pairs, 85 GB, 12
  subdirectories. Camera JPEGs are 6192×4128 (25.6 MP), baseline, 4:2:0 subsampling.
- Rust 1.94.1.
- **This sandbox has no display and no Vulkan.** The GUI can be compiled and unit-tested
  here but cannot be run. Interactive verification must happen on the user's machine.
  Non-GUI logic (scan, grouping, decode, ICC, trash) is testable here.

## Verified against real files

`cargo run --release --bin probe -- <files>` output, confirming the metadata paths:

- `A6709605.ARW` — orientation **8** read from the container; three previews found; the
  6192×4128 / 5.00 MB one correctly selected.
- `A6709605.JPG` — orientation **8**, `ColorSpace: sRGB`, no embedded ICC.
- `A6701135_Export.jpg` — **748-byte ICC** profile extracted, orientation 1, 4740×3164.

## Key bindings

| key | action |
|-----|--------|
| `→` `↓` `Space` | next image |
| `←` `↑` | previous image |
| `PgDn` / `PgUp` | jump 10 |
| `Home` / `End` | first / last |
| **`Delete`** | arm delete (asks to confirm) |
| **`Enter`** | confirm the armed delete |
| `Esc` | cancel an armed delete, else quit |
| click a folder row | collapse / expand it |
| `U` | undo the last deletion |
| `X` | toggle fit mode (refit ↔ keep zoom) |
| `F` | fit to window |
| `Z` | zoom 1:1 |
| wheel | zoom at cursor |
| left-drag | pan |

Delete is deliberately confirmed with `Enter`: it is the one action with real
consequences. Files go to the trash and `U` undoes, but a wrong keystroke during fast
culling should not be silently destructive. `Delete` alone only arms it, and navigating
away disarms it again.

## Test coverage

**194 tests, zero clippy warnings.**

| suite | count | what it covers |
|-------|-------|----------------|
| unit (`--lib`) | 173 | TIFF/EXIF parsing, decode, buffer pooling, grouping, prefetch ring, startup warm-up, view maths, sidebar scroll maths, ICC, trash/undo, app state |
| `tests/sidebar_scroll.rs` | 6 | R21 through a real `egui::Context`: actual row rects inside the actual viewport, wheel scrolling, follow-on-move |
| `tests/real_library.rs` | 11 | the real library: scan consistency, real decodes, RAW/JPEG agreement, ring warm-up, navigation, corrupt input |
| `tests/shader.rs` | 4 | WGSL parses and validates via naga; entry points and bindings match `gpu.rs` |

`tests/fixtures/tiny.jpg` is a real 17×9 JPEG, added because the whole real-library suite
skips wherever the photos are absent — which is everywhere except the user's machine. It
gives the mapped read path in `FileLoader` and the header-probe path behind R14 coverage
that runs in CI and in the sandbox.

The shader suite exists because wgpu only compiles WGSL at pipeline creation, which needs a
device; validating with naga catches shader errors here instead of on the user's machine.

Robustness is covered by feeding every truncation and assorted bit-flips of a real 25 MP
JPEG through the parsers, asserting only that nothing panics.

## What could NOT be tested here

This sandbox has **no display and no Vulkan**. Note that egui itself is pure CPU, so
sidebar *layout* can and now must be tested here (`tests/sidebar_scroll.rs`); reasoning
about egui's geometry instead of exercising it is what let R21 ship broken three times.
What genuinely cannot run here:

- Window creation, surface configuration, swapchain acquire/present.
- Actual texture upload and the draw call.
- egui interaction: clicking tree rows, the mode button, the undo button.
- Whether images visibly look correct, and whether zoom and pan feel right.

Startup failures degrade cleanly rather than panicking: no display, no adapter, no device
and no surface each print a diagnostic and exit 1.

## Changelog

- **Added R23 (fast startup): launch is now ~1 ms and flat in library size**, from seconds
  on a large root. Three separate costs, all of which ran before the window was created:
  one `stat` per file inside `scan` (4287 of them on a cold 85 GB library — the reason it
  scaled with image count), the tree walk itself (`~/Photos` rather than one shoot), and a
  full ±10 prefetch window of 11 decodes and ~1.1 GB of allocation. Sizes and timestamps
  now come from a `MetaStore` filled behind the UI, the scan runs on a worker and arrives
  as an event, and the ring warms one image until the first frame is up. The wgpu instance
  no longer probes every backend, and `RUST_LOG=info` prints a phase breakdown.
- **Delete is now plain `Delete`, not `Shift+Delete`** (R15), at the user's request. Still
  confirmed with `Enter`, still trashed rather than removed, still undoable.
- **Fixed R21 for real, and added a headless egui layout suite.** Three bugs, not one:
  sidebar rows **wrapped to two lines** (56 monospace columns need 446 px; the panel gave
  364), so they were 32 px against the 18 px the virtualised list assumed and the
  offset-to-row mapping was out by ~2×; the pitch double-counted `item_spacing`; and the
  correction re-applied every frame, so the list snapped back whenever the user scrolled
  and pinned entirely near the end. Rows are now truncated to one line, `SIDEBAR_WIDTH` is
  480 so the columns fit, and the tree follows the selection only when it moves.
  `tests/sidebar_scroll.rs` runs the real sidebar through a real `egui::Context` and
  asserts on actual widget rects — the first two bugs are invisible to any test of the
  arithmetic alone.
- **Performance and correctness pass.** Five fixes, three of them cases where the code
  did not do what this document already claimed:
  - **R19's exemption for the on-screen image was never implemented.** The band budget
    applied to it too, so a cold image spent ~66 ms blank while its own upload was
    throttled to protect a frame that was not drawing anything. ~50–65 ms off every cold
    arrival.
  - **R10's colour guard could not fire.** Both callers passed `icc_profile: None`, so a
    wide-gamut file would have been rendered wrong silently — the exact outcome the
    requirement exists to prevent. Added `App::note_colour`, fed from the texture entry.
  - **R14 missed the first image.** Nothing selects index 0 at startup, so image 0 alone
    still fitted a placeholder and then visibly jumped.
  - Added **R22**: map files instead of reading them whole (32 MB → ~5.5 MB per ARW), and
    pool the 102 MB decode buffers (measured 23.8 ms → 1.2 ms per image).
  - The bind group was rebuilt **every frame**; it is now built once per texture.
    `desired_maximum_frame_latency` dropped to 1, removing a vsync of input lag.
  - Deleted `App::scroll_to_selection`, written from three places and read from none —
    R21 is implemented through the carried scroll offset instead.
- **Rewrote texture upload as banded transfers.** The real cause of the lock-ups was that a
  single `write_texture` of 102 MB costs ~14 ms on the main thread, so no per-frame *count*
  limit could ever bound it. Rows are now written ~4 MB at a time under a 4 ms budget,
  with drawing gated on completion. Also throttled the decode target during held keys to
  ~4/s, since chasing 100+ key events per second meant no decode ever completed. Added R21
  for tree auto-scroll, which needed a minimal-correction approach because `scroll_to_me`
  is useless on a virtualised list.
- Added R17-R20: manual zoom leaves refit mode, held-key fast scroll, a wall-clock upload
  budget, and a virtualised sidebar. Together these address the remaining stalls: the
  causes were a 14 ms texture upload done twice per frame, decoders saturating every core,
  a full prefetch window queued per keystroke while a key was held, and 538 widget layouts
  per frame in the tree.
- **Fixed "clicking a row sometimes shows nothing, permanently."** Two bugs compounding:
  the texture cache only retained ±11 around the cursor, so a hovered row far away had its
  texture evicted the instant after upload; and because the renderer takes ownership of the
  pixels on upload, the ring had already marked the slot `Collected` and would never redo
  the work. Fixed by exempting the hinted index from eviction and adding
  `Prefetcher::require`, a repair path used whenever the renderer finds it has no texture
  for the current image. `Cache` mutators now return evicted values so nothing is dropped
  silently.
- **Fixed the UI seizing up over time.** `ui::draw` called `prefetch().state(index)` once
  per row, and a `CollapsingHeader` body iterates every group in the directory — up to
  **538 mutex acquisitions per frame** on the largest folder, contending directly with the
  decode workers. Now one `failed_indices()` snapshot per frame. Separately, textures are
  **pooled and reused**: every frame in a shoot is the same size, so the program was
  creating and destroying 102 MB textures continuously as the window moved.
- Added R14 (no resize on load), R15 (delete confirmation) and R16 (hover debounce).
- **Fixed the aspect-ratio ("squished images") bug.** `clip_transform` was correct, but the
  renderer drew across the whole surface while passing the sidebar-excluded width, so
  everything was stretched horizontally by `window / (window - sidebar)` — 21% at 1600 px.
  The image pass now calls `set_viewport` on the exact rect egui reports as unclaimed
  (`available_rect_before_wrap`), which also handles the sidebar being resizable, since the
  old code hardcoded a *default* width. Three regression tests assert on-screen aspect
  equals image aspect across area shapes, orientations and zoom levels.
- Added R12 (hover preload) and R13 (asynchronous UI). Sidebar rows now show kind
  (`JPG+ARW`), total group size and capture time; the scroll area no longer shrink-wraps,
  so the scrollbar sits at the panel edge.
- Implemented the renderer (wgpu + winit), egui sidebar, deletion/trash/undo and the
  sRGB texture pipeline. **Resolved R10 by analysis: no colour-conversion code was
  written**, because every file in the library is sRGB and the display is unprofiled.
  Added R11 after the user confirmed concurrent editing. 125 tests, clippy clean.
  Two bugs found and fixed while testing the ring (an empty-collection hang and a
  double-publish that silently deleted a decoded image), plus a sidebar index bug where
  a collapsed directory misaligned every later row.
- Recorded R1 benchmark results. **Dropped DCT scaling and the reload-on-zoom requirement
  in R5** — full-res decode measured faster and simpler. Selected zune-jpeg as the
  decoder. Confirmed ARW previews decode cheaper than sidecar JPEGs.
- Initial version. Recorded verified findings on ARW embedded previews, camera JPEG colour
  space, orientation spread, and sidecar naming.
