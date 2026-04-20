# ARC brand assets — drop-in slot

This folder is where the **signed brand pack** from brandpad.io/arc lives.
The app looks for these four filenames at import time. Drop them in and
the next build (`npm run build`) picks them up automatically.

## Required files

| Filename | What it is | Used where |
|---|---|---|
| `arc-logo.svg` | Full `arc` wordmark on transparent bg, monochrome | Titlebar, onboarding hero |
| `arc-logo-white.svg` | Same wordmark, forced white fill (for colored containers) | App icon, sidebar mark |
| `arc-logo-mark.svg` | Just the arc-shape device (circle-square crop, no wordmark) | Favicon, compact titlebar |
| `arc-logo-full-gradient.svg` | Wordmark inside the brand gradient container (hero version) | Launch splash, marketing screens |

All four must be SVG. If you only have PNGs, put them next to this README
with the same base names (`arc-logo.png`, etc.) and the importer will use
those instead — SVG is preferred for crispness at any size.

## App icon (platform-specific)

For the **Tauri app icon** (dock, taskbar, Play Store listing, macOS .app,
Windows .exe), drop:

- `icon-512.png` (512×512, transparent or on the brand-colored container)
- `icon-1024.png` (1024×1024 — Apple requires this for iOS)

When these exist, `src-tauri/icons/gen_icons.py` will use them instead of
the programmatic wordmark fallback it currently generates.

## How to export from brandpad

1. Open brandpad.io/arc → **Download assets**
2. Grab the "Logo" and "App icon" zips
3. Unzip next to this README
4. Rename to match the filenames above (brandpad's exports may use
   `arc_wordmark_RGB.svg` or similar — just rename to `arc-logo.svg`)

## Fallback behavior (no assets present)

The app currently ships with a CSS-rendered `arc` wordmark on a solid blue
square. It's brand-aligned but not pixel-perfect to the signed pack. The
moment you drop the SVGs here, the `<LogoMark>` / `<Wordmark>` components
switch to the real assets.

## Verification after dropping assets

```bash
npm run build            # confirms all 4 SVGs parsed + bundled
npm test -- visual       # regression-tests against brand gradient snapshot
npm run tauri:build      # regenerates Tauri app icons from icon-512.png
```
