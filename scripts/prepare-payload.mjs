#!/usr/bin/env node
// Prepare the bundled payload for the launcher app:
//   1. resources/server/  ← @tianshu-ai/tianshu + its production
//      node_modules (the full server/UI stack).
//   2. resources/bridge/  ← @tianshu-ai/local-bridge + its production
//      node_modules (optional sidecar for Local Bridge mode).
//   3. src-tauri/binaries/node-<target-triple>[.exe] ← Node runtime
//      sidecar via Tauri's externalBin convention.
//
// Mirrors bridge-desktop's prepare-payload.mjs; the diff is one extra
// payload (the server), which bloats install size — but Yu chose
// bundling over launcher-download mode for offline/zero-friction setup.
//
// Usage:
//   node scripts/prepare-payload.mjs [--server-version <semver|latest>]
//                                    [--bridge-version <semver|latest>]
//                                    [--node-version v22.x.y]

import { execSync, execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import https from "node:https";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
const srcTauri = path.join(root, "src-tauri");

const args = process.argv.slice(2);
function argOf(name, def) {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : def;
}
const SERVER_VERSION = argOf("--server-version", "latest");
const BRIDGE_VERSION = argOf("--bridge-version", "latest");
const NODE_VERSION = argOf("--node-version", process.version);

// ─── target triple (matches rustc / Tauri externalBin naming) ───────

function rustTargetTriple() {
  if (process.env.TARGET_TRIPLE) return process.env.TARGET_TRIPLE;
  try {
    const out = execSync("rustc -vV", { encoding: "utf8" });
    const m = out.match(/host:\s*(\S+)/);
    if (m) return m[1];
  } catch { /* fall through */ }
  const p = process.platform;
  const a = process.arch;
  if (p === "darwin") return a === "arm64" ? "aarch64-apple-darwin" : "x86_64-apple-darwin";
  if (p === "win32") return "x86_64-pc-windows-msvc";
  return a === "arm64" ? "aarch64-unknown-linux-gnu" : "x86_64-unknown-linux-gnu";
}

// ─── payload installers ─────────────────────────────────────────────

/** Install an npm package + its production deps into resources/<name>/.
 *  Writes an ESM shim entry so the sidecar can `node index.js`.
 *
 *  `entry`: 'bin' uses the package's bin field (bridge-style CLI that
 *  itself boots the server when run with no args). 'server' forces
 *  dist/index.js — needed for @tianshu-ai/tianshu, whose bin is
 *  'tianshu' (a service manager that prints help without args). */
function installPackagePayload(subdir, spec, label, entry = "bin") {
  const dest = path.join(srcTauri, "resources", subdir);
  fs.rmSync(dest, { recursive: true, force: true });
  fs.mkdirSync(dest, { recursive: true });

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), `${subdir}-payload-`));
  console.log(`[payload] installing ${spec} \u2026`);
  fs.writeFileSync(
    path.join(tmp, "package.json"),
    JSON.stringify({ name: "x", private: true }),
  );
  // --legacy-peer-deps mirrors tianshu's root .npmrc so a strict peer
  // mismatch in a plugin dep doesn't fail the payload bake.
  execSync(
    `npm install --omit=dev --no-audit --no-fund --legacy-peer-deps ${spec}`,
    { cwd: tmp, stdio: "inherit" },
  );

  // Discover the installed package name from the spec
  const pkgName = spec.replace(/@[^@/]+$/, ""); // strip trailing @version
  const pkgDir = path.join(tmp, "node_modules", ...pkgName.split("/"));

  cpDir(path.join(tmp, "node_modules"), path.join(dest, "node_modules"));
  // Entry shim so Rust can spawn `node index.js` regardless of layout.
  let binRel;
  if (entry === "server") {
    // Force dist/index.js — the server entrypoint. @tianshu-ai/tianshu's
    // 'bin' field points at a CLI (service manager), not the server
    // process the launcher needs to spawn directly.
    binRel = path.relative(tmp, path.join(pkgDir, "packages", "server", "dist", "index.js")).replace(/\\/g, "/");
    if (!fs.existsSync(path.join(tmp, binRel))) {
      throw new Error(`server entry not found: ${binRel}`);
    }
  } else {
    const binField = readBinField(pkgDir);
    binRel = binField
      ? path.relative(tmp, path.join(pkgDir, binField)).replace(/\\/g, "/")
      : path.relative(tmp, path.join(pkgDir, "dist", "index.js")).replace(/\\/g, "/");
  }
  fs.writeFileSync(
    path.join(dest, "index.js"),
    `import "./${binRel}";\n`,
  );
  fs.writeFileSync(
    path.join(dest, "package.json"),
    JSON.stringify(
      { name: `tianshu-${subdir}-payload`, private: true, type: "module" },
      null,
      2,
    ),
  );
  console.log(`[payload] ${label} \u2192 ${dest}`);
}

function readBinField(pkgDir) {
  try {
    const pkg = JSON.parse(fs.readFileSync(path.join(pkgDir, "package.json"), "utf8"));
    if (!pkg.bin) return undefined;
    if (typeof pkg.bin === "string") return pkg.bin;
    // Take the first bin entry
    const first = Object.values(pkg.bin)[0];
    return typeof first === "string" ? first : undefined;
  } catch {
    return undefined;
  }
}

function cpDir(src, dst) {
  if (!fs.existsSync(src)) return;
  fs.cpSync(src, dst, { recursive: true });
}

// ─── Node sidecar (same download+extract as bridge-desktop) ─────────

async function prepareNode() {
  const triple = rustTargetTriple();
  const binDir = path.join(srcTauri, "binaries");
  fs.mkdirSync(binDir, { recursive: true });
  const ext = process.platform === "win32" ? ".exe" : "";
  const destBin = path.join(binDir, `node-${triple}${ext}`);

  const ver = NODE_VERSION.startsWith("v") ? NODE_VERSION : `v${NODE_VERSION}`;
  const { url, inner } = nodeDownload(ver);
  console.log(`[payload] fetching Node ${ver} from ${url}`);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "node-dl-"));
  const archive = path.join(tmp, path.basename(url));
  await download(url, archive);
  // Use execFileSync (argv, no shell) so paths with spaces or odd
  // chars don't need per-platform quoting. POSIX 'quote()' uses single
  // quotes which Windows CreateProcess reads literally and chokes on.
  if (url.endsWith(".zip")) {
    // Windows 10 1803+ ships tar.exe (bsdtar) in system32 which
    // handles zip natively. 'unzip' only exists via Git Bash on
    // Windows. Try tar first, fall back to PowerShell Expand-Archive.
    if (process.platform === "win32") {
      try {
        execFileSync("tar", ["-xf", archive, "-C", tmp], { stdio: "inherit" });
      } catch {
        execFileSync(
          "powershell",
          [
            "-NoProfile",
            "-Command",
            `Expand-Archive -Force -Path "${archive}" -DestinationPath "${tmp}"`,
          ],
          { stdio: "inherit" },
        );
      }
    } else {
      execFileSync("unzip", ["-o", "-q", archive, "-d", tmp], { stdio: "inherit" });
    }
  } else {
    execFileSync("tar", ["-xf", archive, "-C", tmp], { stdio: "inherit" });
  }
  const extracted = path.join(tmp, inner);
  fs.copyFileSync(extracted, destBin);
  if (ext === "") fs.chmodSync(destBin, 0o755);
  console.log(`[payload] node sidecar \u2192 ${destBin}`);

  // Also bundle npm from the Node.js distribution so the launcher can
  // run `node npm-cli.js install` without requiring a system npm/npx.
  const nodeRoot = path.dirname(path.dirname(extracted)); // e.g. node-v22.x.x-darwin-arm64
  // On Windows the layout is flat: node-vXX-win-x64/{node.exe, npm, npm.cmd, node_modules/npm/}
  // On Unix it's: node-vXX-<os>-<arch>/lib/node_modules/npm/
  const npmSrcUnix = path.join(tmp, inner.split("/")[0], "lib", "node_modules", "npm");
  const npmSrcWin = path.join(tmp, inner.split("/")[0], "node_modules", "npm");
  const npmSrc = fs.existsSync(npmSrcUnix) ? npmSrcUnix : npmSrcWin;
  const npmDest = path.join(srcTauri, "resources", "npm");
  if (fs.existsSync(npmSrc)) {
    fs.rmSync(npmDest, { recursive: true, force: true });
    cpDir(npmSrc, npmDest);
    console.log(`[payload] npm cli \u2192 ${npmDest}`);
  } else {
    console.warn(`[payload] WARN: npm not found in Node distribution at ${npmSrcUnix} or ${npmSrcWin}`);
  }
}

function nodeBase() {
  const m = (process.env.NODE_MIRROR || "https://nodejs.org/dist").replace(/\/+$/, "");
  return m;
}

function nodeDownload(ver) {
  const base = nodeBase();
  const p = process.platform;
  const a = process.arch;
  if (p === "win32") {
    const arch = a === "arm64" ? "arm64" : "x64";
    return {
      url: `${base}/${ver}/node-${ver}-win-${arch}.zip`,
      inner: `node-${ver}-win-${arch}/node.exe`,
    };
  }
  if (p === "darwin") {
    const arch = a === "arm64" ? "arm64" : "x64";
    return {
      url: `${base}/${ver}/node-${ver}-darwin-${arch}.tar.gz`,
      inner: `node-${ver}-darwin-${arch}/bin/node`,
    };
  }
  const arch = a === "arm64" ? "arm64" : "x64";
  return {
    url: `${base}/${ver}/node-${ver}-linux-${arch}.tar.xz`,
    inner: `node-${ver}-linux-${arch}/bin/node`,
  };
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    https
      .get(url, (res) => {
        if (res.statusCode && res.statusCode >= 300 && res.headers.location) {
          file.close();
          download(res.headers.location, dest).then(resolve, reject);
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode} for ${url}`));
          return;
        }
        res.pipe(file);
        file.on("finish", () => file.close(() => resolve()));
      })
      .on("error", reject);
  });
}

function quote(s) {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

// ─── run ────────────────────────────────────────────────────────────

(async () => {
  installPackagePayload(
    "server",
    `@tianshu-ai/tianshu@${SERVER_VERSION}`,
    "tianshu server",
    "server",
  );
  installPackagePayload(
    "bridge",
    `@tianshu-ai/local-bridge@${BRIDGE_VERSION}`,
    "local-bridge",
  );

  // @playwright/mcp is the browser-engine MCP server that local-bridge
  // shells out to. It's not a direct dependency of local-bridge (it's
  // resolved at runtime via resolveEmbeddedMcp), so npm install won't
  // pull it automatically. Install it into the bridge payload so the
  // bundled bridge can find it without needing npx (which isn't
  // available in the Tauri .app / .exe — we only ship a bare node).
  console.log("[payload] installing @playwright/mcp into bridge payload\u2026");
  execSync(
    "npm install --omit=dev --no-audit --no-fund --legacy-peer-deps @playwright/mcp@latest",
    { cwd: path.join(srcTauri, "resources", "bridge"), stdio: "inherit" },
  );
  await prepareNode();
  console.log("[payload] done.");
})().catch((e) => {
  console.error("[payload] failed:", e);
  process.exit(1);
});
