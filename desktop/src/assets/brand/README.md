# ARC brand assets

The app draws the arc wordmark from the real brand file, never from a font.

## In use

| Filename | What it is | Used where |
|---|---|---|
| `arc-logo-white.svg` | The arc.ai wordmark, white on transparent (viewBox `0 0 241.87 94.04`). The same file the website ships as `assets/arc-logo-white.svg`. | Sidebar (`<Wordmark>`), titlebar, and the logo icon (`<LogoMark>`: the wordmark in a solid or gradient container) on onboarding and the error screen |

`src/components/Logo.tsx` imports this file directly, so a build without it
fails instead of falling back to a placeholder. The tagline is "Own your AI"
(`<Tagline>`).

## Updating the wordmark

Replace `arc-logo-white.svg` with the new export from the brand pack, keeping
the file name and a white fill. If the viewBox changes, update
`WORDMARK_ASPECT` in `Logo.tsx` to match.

## App icon (platform-specific)

For the **Tauri app icon** (dock, taskbar, macOS .app, Windows .exe), drop:

- `icon-512.png` (512×512, transparent or on the brand-colored container)
- `icon-1024.png` (1024×1024 - Apple requires this for iOS)

When these exist, `src-tauri/icons/gen_icons.py` will use them instead of
the programmatic wordmark fallback it currently generates.

## Verification after changing assets

```bash
npm run build            # confirms the SVG parsed and bundled
npm test -- visual       # checks the logo icon and tagline render
npm run tauri:build      # regenerates Tauri app icons from icon-512.png
```
