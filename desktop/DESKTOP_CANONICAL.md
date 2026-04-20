# ARC Node — Canonical Desktop App

**Version:** 0.1.0 · **Stack:** Tauri 2 + React 18 + Vite 5 · **Status:** Working

This is *the* desktop app TJ built and approved on 2026-04-19. Fully committed to
the repo — if the source ever disappears from the working tree again, check
`desktop/src/` at any commit after this one.

## Running

```bash
cd desktop
npm install
npm run tauri:dev     # hot-reload dev build
npm run tauri:build   # release .dmg → src-tauri/target/release/bundle/dmg/
```

## Why the app may appear blank
- Vite dev server (port 1420) not running → `beforeDevCommand` starts it
- Missing icons → run `python3 src-tauri/icons/gen_icons.py`
- Wrong `@tauri-apps/cli` major version in `node_modules/` → `rm -rf node_modules && npm install`

## Draggable window
- `src/styles/app.css` sets `.titlebar { -webkit-app-region: drag }`
- `src/components/Titlebar.tsx` includes `data-tauri-drag-region` attribute
- `tauri.conf.json` uses `titleBarStyle: "Overlay"` + `decorations: true` — native drag + traffic lights on macOS
