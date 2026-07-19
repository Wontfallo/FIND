# FIND — instant file search for your PC

FIND is a fast, native desktop search tool in the spirit of
[Everything](https://www.voidtools.com/): type a few characters and see every
matching file and folder on your machine instantly. It's a single small
executable written in Rust — no runtimes, no services, no installers.

## Features

- **Instant as-you-type search** over an in-memory index of your whole PC
- **Three match modes**: substring (default), **fuzzy** (`rpt2024` finds
  `report_2024.pdf`), and full **regex**
- **Everything-style filters**, mixable with plain search words:

  | Filter | Example | Meaning |
  |---|---|---|
  | `ext:` | `ext:pdf,docx` | only these extensions |
  | `path:` | `path:projects` | full path must contain this |
  | `size:` | `size:>10mb`, `size:1mb..100mb` | file size |
  | `date:` | `date:>2024-01-01`, `date:2024-01..2024-06` | modified date |
  | `type:` | `type:folder`, `type:file` | folders or files only |
  | `content:` | `content:"todo"` | **grep inside text files** |

- **Category chips**: Documents, Images, Audio, Video, Archives, Code,
  Executables, Folders — one click to narrow results
- **Preview pane**: text files and images preview in-app; content-search hits
  show the matching line
- **Actions**: double-click / Enter to open, right-click for **Open location**
  (reveals in Explorer), **Copy full path**, copy name, copy folder
- **Sortable columns**: name, path, size, modified — click headers
- **Live index**: a filesystem watcher keeps results current as files are
  created, renamed, or deleted; the index is also saved to disk so startup is
  instant, with a background refresh scan
- **Case toggle**, configurable roots and exclusions, max-results cap

## Getting it

Grab a build from the **Actions** tab (every push builds Windows, Linux, and
macOS binaries) or from **Releases** for tagged versions. On Windows it's a
single `FIND-windows-x64.exe` — no install needed.

Or build from source with [Rust](https://rustup.rs/):

```sh
cargo build --release
# binary at target/release/find (find.exe on Windows)
```

## Usage tips

- First launch scans your drives in the background — you can search while it
  runs; the status bar shows progress. Later launches load the saved index
  instantly and refresh it quietly.
- `report ext:pdf size:>1mb path:2024` — combine anything.
- `content:"connection string" ext:cs,json` greps only inside matching text
  files, powered by the ripgrep engine, so it's fast and skips binaries.
- **Esc** clears the search. **↑/↓** move the selection, **Enter** opens,
  **Ctrl+Shift+C** copies the full path.
- Settings (⚙) lets you choose which folders/drives are indexed and exclude
  noisy paths (`node_modules`, caches, …). "Save & Rescan" applies it.
- On Windows, indexing all drives with this generic scanner takes longer than
  Everything's NTFS-specific MFT reading (minutes rather than seconds on large
  drives) — but it only happens in the background, and searches stay instant
  thanks to the saved index.

## Architecture (for the curious)

- `src/index.rs` — Everything-style index: each entry stores just its name and
  a parent pointer, so millions of files fit in a few hundred MB and full
  paths are reconstructed on demand. Parallel scan via `jwalk`, persisted with
  `bincode`.
- `src/query.rs` / `src/search.rs` — query parser and a rayon-parallel
  matcher (substring scoring, `nucleo` fuzzy matching, regex), with
  generation-counter cancellation so stale keystrokes never block fresh ones.
- `src/content.rs` — content grep with `grep-searcher` (ripgrep's engine),
  binary detection, and size caps.
- `src/watcher.rs` — `notify`-based live updates, batched every 500 ms.
- `src/app.rs` — egui UI; all search work happens on worker threads, the UI
  never blocks.

License: MIT
