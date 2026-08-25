# cull

A fast viewer for culling photo shoots. It shows one image at a time, keeps the
neighbouring ones decoded in the background so navigation is instant, and deletes
JPG+RAW pairs as a single unit.

It is deliberately not a photo manager — no tagging, ratings, editing or thumbnail
database. Linux only, and it needs a Vulkan-capable GPU.

## Building

```
cargo build --release
./target/release/cull ~/Photos/2026-lofoten
```

The directory is scanned recursively. Pointing it at a large root is fine; the window
opens immediately and the tree fills in behind it.

## How it works

Files are grouped by basename, so `A6709605.JPG`, `A6709605.ARW` and any RawTherapee
`.pp3` sidecars appear as one row and are deleted together. RAW files are displayed from
the full-size JPEG preview embedded in the file, which decodes faster than the sidecar
JPEG and avoids demosaicing entirely.

A 25 MP frame takes roughly 170 ms to decode, which is far too slow to do on a keypress,
so a window of ±10 images around the cursor is kept decoded at all times. Switching to
one of them is a texture bind. Deletions go to the freedesktop trash and can be undone,
64 deep.

## Keys

| | |
|---|---|
| `→` `↓` `Space` | next image |
| `←` `↑` | previous image |
| `PgDn` `PgUp` | jump ten |
| `Home` `End` | first, last |
| `Delete` | mark for deletion |
| `Enter` | confirm it |
| `Esc` | cancel, or quit if nothing is marked |
| `U` | undo the last deletion |
| `X` | toggle refit / keep zoom |
| `F` | fit to window |
| `Z` | zoom 1:1 |
| wheel | zoom at the cursor |
| left-drag | pan |

Deleting takes two keystrokes on purpose. Everything goes to the trash and undo works,
but it is still the one action with real consequences. Navigating away cancels a pending
delete, so a stray `Enter` can't remove something you have already moved past.

Keep-zoom mode is for focus checking: the zoom level and pan position carry across image
switches, so you can flick between two frames of the same subject at 100%.

## Notes

`RUST_LOG=info` prints startup and scan timings.

`REQUIREMENTS.md` records the design decisions and the measurements behind them, including
the ones that were tried and dropped.
