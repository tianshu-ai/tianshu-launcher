# Tianshu Launcher

One-click desktop app that bundles everything you need to run Tianshu:
the server, a Node 22 runtime, and the Local Bridge sidecar — all
inside a single native installer for Windows, macOS, and Linux.

## Why

Running `@tianshu-ai/tianshu` the npm way requires Node 22+ installed
and a terminal to run `tianshu start`. That's a lot of friction for
non-developer users. The launcher skips all of it:

1. Download the installer for your OS.
2. Open the app.
3. Server starts automatically. Click **Open Tianshu** to use it.
4. Toggle **Local Bridge** on if you're on Windows (where native
   sandboxes aren't available) or want your browser/shell exposed to
   the agent.

No Node install, no `npm`, no CLI. Launcher stays running in the tray.

## Architecture

- **Tauri 2** app (Rust shell + system WebView + a small HTML UI).
- `scripts/prepare-payload.mjs` downloads the published `@tianshu-ai/tianshu`
  and `@tianshu-ai/local-bridge` packages at build time and bakes them
  into `src-tauri/resources/{server,bridge}/`.
- The same script downloads Node 22 for the target triple and installs
  it as a Tauri `externalBin` sidecar at `src-tauri/binaries/node-<triple>[.exe]`.
- At runtime Rust spawns `node resources/<name>/index.js` for each
  child process.
- Config and data use the same locations as the npm install:
  `~/.tianshu/` on macOS/Linux, `%LOCALAPPDATA%\tianshu\` on Windows.
  Installing the launcher next to an existing `npm i -g` install is
  safe — they share state.

## Install

Download the latest installer from
[GitHub Releases](https://github.com/tianshu-ai/tianshu-launcher/releases).

### macOS — "App is damaged" / Gatekeeper warning

The app is not yet code-signed with an Apple Developer certificate.
macOS Gatekeeper will block it the first time you open it. Fix:

```bash
/usr/bin/xattr -cr /Applications/Tianshu.app
```

If you get `option -r not recognized` (Python xattr shadowing the
system one), use:

```bash
find /Applications/Tianshu.app -exec /usr/bin/xattr -c {} +
```

Then double-click to open normally. This only needs to be done once.

### Windows — "Windows protected your PC" (SmartScreen)

The installer is not yet code-signed. SmartScreen may show a warning.
Click **More info → Run anyway** to proceed.

### Linux

No signing issues. Install the `.deb` package directly.

## Build

```bash
# Prerequisites: Node 22+ (for the payload script), Rust toolchain,
# platform build deps per Tauri docs.
npm install
npm run payload   # downloads server + bridge + Node sidecar
npm run build     # produces msi/nsis/dmg/appimage via tauri build
```

The `beforeBuildCommand` in `tauri.conf.json` runs `prepare-payload.mjs`
automatically before `tauri build`, so a plain `npm run build` works
too — the step above is only split out for easier debugging.

## Related

- [`tianshu-ai/tianshu`](https://github.com/tianshu-ai/tianshu) — the
  server + web UI this launcher bundles.
- [`tianshu-ai/local-bridge`](https://github.com/tianshu-ai/local-bridge) —
  the Node client that exposes a user's browser and shell to the server
  over WebSocket.
- [`tianshu-ai/bridge-desktop`](https://github.com/tianshu-ai/bridge-desktop) —
  the standalone Local Bridge desktop app (for connecting to a remote
  Tianshu server). The launcher's bundling approach mirrors this one.
