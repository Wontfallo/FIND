# DOOM in your browser

The original DOOM (1993), playable in a web browser. This is
[Chocolate Doom](https://www.chocolate-doom.org/) compiled to WebAssembly
(the [cloudflare/doom-wasm](https://github.com/cloudflare/doom-wasm) build)
running the freely-distributable shareware episode,
*Knee-Deep in the Dead*.

## Play

Browsers won't load WebAssembly from `file://` URLs, so serve the folder over
HTTP — any static server works:

```sh
cd doom
python3 -m http.server 8000
# then open http://localhost:8000/
```

or with Node:

```sh
npx serve doom
```

The page also works as-is on any static host (GitHub Pages, Netlify, etc.).

## Controls

| Key | Action |
|---|---|
| `W` `A` `S` `D` / arrow keys | move / strafe |
| Mouse | aim |
| `Space` / left click | fire |
| `E` | use (open doors, flip switches) |
| `Shift` | run |
| `1`–`7` | switch weapon |
| `Tab` | automap |
| `Enter` / `Esc` | menus |

Click the game once so it has keyboard focus. Sound starts after your first
click or keypress (browsers block audio until a user gesture). Music is
disabled in this build; sound effects work.

## Files

| File | What it is |
|---|---|
| `index.html` | game page and Emscripten glue |
| `websockets-doom.js` / `.wasm` | Chocolate Doom engine compiled to WebAssembly |
| `doom1.wad` | DOOM shareware v1.9 game data (episode 1) |
| `default.cfg` | key bindings and engine settings |

## Licensing

- **Engine**: Chocolate Doom, GPL-2.0. WebAssembly build from
  [cloudflare/doom-wasm](https://github.com/cloudflare/doom-wasm), which
  provides the corresponding source code.
- **Game data**: `doom1.wad` is the DOOM shareware episode, © 1993
  id Software. id Software permitted free distribution of the unmodified
  shareware version. The full game's WADs are commercial and are *not*
  included — but if you own DOOM or DOOM II, you can swap in your own
  `doom.wad`/`doom2.wad` and adjust the `-iwad` argument in `index.html`.
