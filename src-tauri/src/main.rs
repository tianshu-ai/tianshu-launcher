// Tianshu Launcher — one-click desktop app that spawns tianshu server
// + optional Local Bridge sidecar from bundled Node runtime.
//
// Design mirrors bridge-desktop's main.rs but with two differences:
//   1. We manage TWO children: the server (always) and the local-bridge
//      sidecar (optional, enabled when the user ticks "Local Bridge").
//   2. The tray + webview UI expose Start/Stop/Open-WebUI + a logs
//      panel; config is otherwise all delegated to the server's own
//      Web UI at http://localhost:3110.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(unix)]
extern crate libc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// Strip the Windows extended-length path prefix (`\\?\`) that Tauri's
/// `resource_dir()` returns. Node.js's module loader cannot handle this
/// prefix — `realpathSync('C:')` crashes with EISDIR.
#[cfg(windows)]
fn strip_unc_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p
    }
}
#[cfg(not(windows))]
fn strip_unc_prefix(p: PathBuf) -> PathBuf { p }

use serde::{Deserialize, Serialize};
use tauri_plugin_updater::UpdaterExt;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};

/// Handles to tray MenuItems whose labels change with state. Stored
/// as app-managed state so refresh_tray can mutate them from any
/// thread without re-building the menu.
#[derive(Default)]
struct TrayItems {
    toggle_server: Mutex<Option<MenuItem<tauri::Wry>>>,
    start_all_bridges: Mutex<Option<MenuItem<tauri::Wry>>>,
    stop_all_bridges: Mutex<Option<MenuItem<tauri::Wry>>>,
    restart_server: Mutex<Option<MenuItem<tauri::Wry>>>,
    bridge_submenu: Mutex<Option<Submenu<tauri::Wry>>>,
    /// Per-profile Start/Stop MenuItems. Keyed by profile id so
    /// refresh_tray can toggle the label on each as state changes;
    /// cleared and rebuilt when the profile list itself changes.
    bridge_profile_items: Mutex<HashMap<String, MenuItem<tauri::Wry>>>,
    /// Snapshot of the profile ids currently in the submenu, so we
    /// can detect when a rebuild is needed (added, removed, renamed).
    bridge_profile_signature: Mutex<Vec<(String, String)>>, // (id, name)
}

/// Update the tray icon + dynamic menu labels to reflect current
/// running state. Synchronous — called from inside each start/stop
/// command after it releases its own state locks. Deliberately NOT
/// wired through app.listen() to avoid any chance of the event loop
/// + lock-reacquire deadlocking the webview.
fn refresh_tray(app: &tauri::AppHandle) {
    let state: State<ProcState> = app.state();
    let server_running = state.server.lock().unwrap().is_some();
    let bridge_running_ids: std::collections::HashSet<String> =
        state.bridges.lock().unwrap().keys().cloned().collect();
    let bridge_count = bridge_running_ids.len();
    let any_bridge = bridge_count > 0;
    let active = server_running || any_bridge;

    if let Some(tray) = app.tray_by_id("main") {
        let bytes: &[u8] = if active { ICON_RUNNING } else { ICON_STOPPED };
        if let Ok(img) = tauri::image::Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(img));
        }
    }

    if let Some(items) = app.try_state::<TrayItems>() {
        if let Some(toggle) = items.toggle_server.lock().unwrap().as_ref() {
            let _ = toggle.set_text(if server_running { "Stop Server" } else { "Start Server" });
        }
        if let Some(restart) = items.restart_server.lock().unwrap().as_ref() {
            let _ = restart.set_enabled(server_running);
        }
        // Start/Stop All only make sense when there are profiles at
        // all, and respectively when at least one is NOT running /
        // IS running.
        let total_profiles = load_bridge_config().profiles.len();
        let any_stopped = total_profiles > bridge_count;
        if let Some(start_all) = items.start_all_bridges.lock().unwrap().as_ref() {
            let _ = start_all.set_enabled(any_stopped);
            let stopped_count = total_profiles - bridge_count;
            let _ = start_all.set_text(if total_profiles > 1 && stopped_count > 1 {
                format!("Start All Bridges ({stopped_count})")
            } else {
                "Start All Bridges".to_string()
            });
        }
        if let Some(stop_all) = items.stop_all_bridges.lock().unwrap().as_ref() {
            let _ = stop_all.set_enabled(any_bridge);
            let _ = stop_all.set_text(if bridge_count > 1 {
                format!("Stop All Bridges ({bridge_count})")
            } else {
                "Stop All Bridges".to_string()
            });
        }

        // Rebuild per-profile menu entries if the profile list changed.
        let profiles = load_bridge_config().profiles;
        let current_sig: Vec<(String, String)> =
            profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
        let mut sig_slot = items.bridge_profile_signature.lock().unwrap();
        if *sig_slot != current_sig {
            rebuild_bridge_submenu(app, &items, &profiles);
            *sig_slot = current_sig;
        }

        // Toggle per-profile labels (Start/Stop) based on live state.
        for (id, mi) in items.bridge_profile_items.lock().unwrap().iter() {
            let running = bridge_running_ids.contains(id);
            if let Some(profile) = profiles.iter().find(|p| &p.id == id) {
                let _ = mi.set_text(format!(
                    "{} {}",
                    if running { "Stop" } else { "Start" },
                    profile.name,
                ));
            }
        }
    }
}

/// Rebuild the per-profile section of the bridge submenu. Keeps the
/// trailing 'Manage Profiles…' + 'Stop All Bridges' + separator and
/// discards whatever is above them.
fn rebuild_bridge_submenu(
    app: &tauri::AppHandle,
    items: &TrayItems,
    profiles: &[BridgeProfile],
) {
    let Some(submenu) = items.bridge_submenu.lock().unwrap().clone() else { return };
    // Clear everything and rebuild from scratch. The 2 tail items are
    // appended last so their order stays stable.
    while let Ok(Some(_)) = submenu.remove_at(0) {}
    items.bridge_profile_items.lock().unwrap().clear();

    // Head: one Start/Stop MenuItem per profile (label refined by the
    // next refresh_tray pass with the real running state).
    if profiles.is_empty() {
        if let Ok(none) = MenuItem::with_id(
            app,
            "bridge_none",
            "(no profiles)",
            false,
            None::<&str>,
        ) {
            let _ = submenu.append(&none);
        }
    } else {
        for p in profiles {
            let id = format!("bridge_toggle_{}", p.id);
            if let Ok(mi) = MenuItem::with_id(app, &id, format!("Start {}", p.name), true, None::<&str>) {
                let _ = submenu.append(&mi);
                items
                    .bridge_profile_items
                    .lock()
                    .unwrap()
                    .insert(p.id.clone(), mi);
            }
        }
    }

    // Tail: separator + Manage Profiles… + Stop All Bridges.
    if let Ok(sep) = PredefinedMenuItem::separator(app) {
        let _ = submenu.append(&sep);
    }
    if let Ok(manage) = MenuItem::with_id(
        app,
        "manage_bridge",
        "Manage Profiles\u{2026}",
        true,
        None::<&str>,
    ) {
        let _ = submenu.append(&manage);
    }
    if let Some(start_all) = items.start_all_bridges.lock().unwrap().as_ref() {
        let _ = submenu.append(start_all);
    }
    if let Some(stop_all) = items.stop_all_bridges.lock().unwrap().as_ref() {
        let _ = submenu.append(stop_all);
    }
}

static ICON_STOPPED: &[u8] = include_bytes!("../icons/tray/stopped.png");
static ICON_RUNNING: &[u8] = include_bytes!("../icons/tray/running.png");

// ─── child-process state ────────────────────────────────────────────

#[derive(Default)]
struct ProcState {
    server: Mutex<Option<Child>>,
    /// Running bridge children keyed by profile id. Multiple profiles
    /// can run concurrently (same model as bridge-desktop).
    bridges: Mutex<HashMap<String, Child>>,
}

// ─── bridge profile config (compatible with bridge-desktop) ─────────
//
// Config lives at ~/.tianshu-bridge/config.json so launcher and the
// standalone bridge-desktop app share the same profile list.

#[derive(Serialize, Deserialize, Clone, Debug)]
struct BridgeProfile {
    #[serde(default = "gen_id")]
    id: String,
    #[serde(default = "default_name")]
    name: String,
    #[serde(default = "default_server")]
    server: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    device: String,
    #[serde(default = "default_true")]
    auto_start: bool,
    #[serde(default = "default_true")]
    browser: bool,
    #[serde(default = "default_engine")]
    engine: String,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    shell: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct BridgeConfig {
    #[serde(default)]
    profiles: Vec<BridgeProfile>,
}

fn default_name() -> String { "Default".into() }
fn default_server() -> String { "ws://localhost:3110/ws".into() }
fn default_true() -> bool { true }
fn default_engine() -> String { "own".into() }

fn gen_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("p_{:x}_{:x}", ms, h.finish() as u32)
}

fn bridge_config_dir() -> PathBuf { home_dir().join(".tianshu-bridge") }

fn bridge_config_path() -> PathBuf { bridge_config_dir().join("config.json") }

fn load_bridge_config() -> BridgeConfig {
    let path = bridge_config_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return BridgeConfig::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_bridge_config(cfg: &BridgeConfig) -> Result<(), String> {
    let dir = bridge_config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(bridge_config_path(), json).map_err(|e| e.to_string())
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Status {
    server_running: bool,
    bridge_running: bool,
    server_port: u16,
}

// ─── bundled payload paths ──────────────────────────────────────────

/// Where the Node sidecar lives after Tauri unpacks externalBin.
/// At dev time it's src-tauri/binaries/; at install time it's the app's
/// Resources dir (handled by tauri::path::resolve_resource).
fn node_sidecar_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let triple = rustc_target_triple();
    // Tauri 2 is inconsistent about how it ships externalBin on
    // different OSes / bundlers:
    //   - macOS .app: Contents/MacOS/node-<triple>  (preserved name)
    //   - macOS .dmg: same as .app
    //   - Windows NSIS .exe: node.exe at app root (STRIPS the triple!)
    //   - Windows MSI: Bin_node.exe at root (prefixed, not what we want)
    //   - Linux .deb: /usr/lib/<app>/node-<triple>  (preserved)
    //   - Linux AppImage: node-<triple> next to the launched binary
    //
    // So we try several name forms + several roots.
    let triple_name = if cfg!(windows) {
        format!("node-{triple}.exe")
    } else {
        format!("node-{triple}")
    };
    let plain_name = if cfg!(windows) { "node.exe" } else { "node" };

    let mut tried: Vec<PathBuf> = Vec::new();
    let probe = |p: PathBuf, out: &mut Vec<PathBuf>| -> Option<PathBuf> {
        if p.exists() {
            Some(p)
        } else {
            out.push(p);
            None
        }
    };

    // 1. Resource dir (where Tauri says it put resources).
    if let Ok(raw_base) = app.path().resource_dir() {
        let base = strip_unc_prefix(raw_base);
        for name in [triple_name.as_str(), plain_name] {
            if let Some(hit) = probe(base.join(name), &mut tried) { return Ok(hit); }
            if let Some(hit) = probe(base.join("binaries").join(name), &mut tried) { return Ok(hit); }
        }
    }

    // 2. Alongside the launcher's own exe (NSIS layout: Tauri drops
    //    node.exe directly next to tianshu-launcher.exe).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in [triple_name.as_str(), plain_name] {
                if let Some(hit) = probe(dir.join(name), &mut tried) { return Ok(hit); }
                if let Some(hit) = probe(dir.join("binaries").join(name), &mut tried) { return Ok(hit); }
            }
        }

        // 3. Dev-mode: walk up from target/<profile>/<exe> to src-tauri/binaries/.
        if let Some(src_tauri) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            for name in [triple_name.as_str(), plain_name] {
                if let Some(hit) = probe(src_tauri.join("binaries").join(name), &mut tried) {
                    return Ok(hit);
                }
            }
        }
    }

    Err(format!("node sidecar not found. Tried: {tried:?}"))
}

/// Where the bundled payload (server + bridge) lives.
/// Resolve the payload entry for 'server' or 'bridge', preferring the
/// user's override directory (~/.tianshu-launcher/overrides/<sub>/
/// index.js) when it exists. That's how runtime payload upgrades
/// work: 'Check for Updates' writes a fresh npm install into the
/// override location, and subsequent spawns pick it up without
/// re-bundling the launcher.
fn resource_payload_path(app: &tauri::AppHandle, sub: &str) -> Result<PathBuf, String> {
    // 1. Prefer runtime-installed override.
    let override_entry = payload_override_dir(sub).join("index.js");
    if override_entry.exists() {
        return Ok(override_entry);
    }

    // 2. Bundled resource dir (production install).
    let base = strip_unc_prefix(app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?);
    let candidate = base.join("resources").join(sub).join("index.js");
    if candidate.exists() {
        return Ok(candidate);
    }

    // 3. Dev-mode fallback: resources/ isn't copied to target/debug.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(src_tauri) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let dev = src_tauri.join("resources").join(sub).join("index.js");
            if dev.exists() {
                return Ok(dev);
            }
        }
    }
    Err(format!("payload entry not found: {candidate:?}"))
}

/// Where runtime-installed payload lives. Separate from bundled
/// resources so a reinstall of the launcher never clobbers the user's
/// upgraded tianshu/bridge versions, and uninstalling the launcher
/// doesn't nuke a known-good override.
fn payload_override_dir(sub: &str) -> PathBuf {
    launcher_data_dir().join("overrides").join(sub)
}

fn launcher_data_dir() -> PathBuf { home_dir().join(".tianshu-launcher") }

/// Cross-platform home directory resolver.
/// Windows uses %USERPROFILE% (HOME is unset in a default install);
/// macOS/Linux use $HOME. Falls back to '.' only in degenerate setups.
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("USERPROFILE") {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("HOME") {
        return PathBuf::from(p);
    }
    PathBuf::from(".")
}

fn rustc_target_triple() -> &'static str {
    // These must match prepare-payload.mjs's rustTargetTriple().
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

// ─── child spawn/kill ───────────────────────────────────────────────


/// Locate the bundled web UI dist directory next to the server payload.
/// The @tianshu-ai/tianshu npm package ships packages/web/dist/ as part of
/// the install; the server mounts it via TIANSHU_WEB_DIST (see
/// packages/server/dist/boot/static-spa.js).
fn web_dist_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // Server shim is at resources/server/index.js and the real web dist
    // sits at resources/server/node_modules/@tianshu-ai/tianshu/packages/web/dist/.
    let server_entry = resource_payload_path(app, "server")?;
    let server_root = server_entry
        .parent()
        .ok_or_else(|| "no parent for server entry".to_string())?;
    let web = server_root
        .join("node_modules")
        .join("@tianshu-ai")
        .join("tianshu")
        .join("packages")
        .join("web")
        .join("dist");
    if web.join("index.html").exists() {
        Ok(web)
    } else {
        Err(format!("web dist not found at {web:?}"))
    }
}

/// Kill a child process and its entire process tree.
/// Windows: taskkill /T /F walks the tree.
/// Unix: killpg sends SIGKILL to the whole process group (child was
/// spawned with setsid so pid == pgid).
fn kill_child_ref(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        let mut kill_cmd = std::process::Command::new("taskkill");
        kill_cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            kill_cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = kill_cmd.output();
    }
    #[cfg(unix)]
    {
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_child(child_opt: &mut Option<Child>) {
    if let Some(ref mut child) = child_opt.take() {
        kill_child_ref(child);
    }
}

/// Spawn a child like spawn_child, but hook up stdout+stderr pumps
/// that write to <launcher_data_dir>/logs/<label>.log. Rotates on
/// each launch (overwrite) so a crash loop can't fill the disk.
///
/// Critical for Windows debugging: tianshu's own log-tee writes to
/// ~/.tianshu/logs/server-*.log, but if the server crashes during
/// its module imports (missing native dep, bad path) the log-tee
/// never gets a chance to install. This gives us a floor-level log.
fn spawn_child_logged(
    node: &PathBuf,
    entry: &PathBuf,
    extra_env: &[(&str, PathBuf)],
    label: &str,
    extra_args: &[String],
) -> Result<Child, String> {
    use std::io::{BufRead, BufReader, Write};

    let log_dir = launcher_data_dir().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{label}.log"));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|e| format!("open {log_path:?}: {e}"))?;

    let mut cmd = Command::new(node);
    cmd.arg(entry);
    for a in extra_args {
        cmd.arg(a);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: don't flash a console window.
        // CREATE_NEW_PROCESS_GROUP: launcher Ctrl+C doesn't propagate
        // to the sidecar; also makes taskkill /T semantics clean.
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        // setsid: child becomes session leader with pid == pgid, so
        // killpg(pid, SIGKILL) at shutdown fells every descendant
        // (Node's workers, playwright, native binaries) at once.
        // Without this, Quit on macOS leaves Node sidecars owning
        // 3110 across launcher restarts.
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;

    // Windows: assign the child to our process-wide Job Object so
    // the OS tears down the whole tree when the launcher exits
    // (including crashes / taskmgr force-kill, since the kernel
    // owns the lifecycle, not us). The job is created once at
    // startup; see ensure_job_object().
    #[cfg(windows)]
    {
        if let Err(e) = assign_to_launcher_job(&child) {
            eprintln!("[warn] AssignProcessToJobObject failed: {e}");
        }
    }

    // Header so a tail of the file shows the latest boot's args.
    {
        let mut hdr = log_file.try_clone().ok();
        if let Some(ref mut f) = hdr {
            let _ = writeln!(
                f,
                "=== {label} launched node={node:?} entry={entry:?} args={extra_args:?} env={:?} ===",
                extra_env.iter().map(|(k, v)| format!("{k}={v:?}")).collect::<Vec<_>>(),
            );
        }
    }

    if let Some(pipe) = child.stdout.take() {
        let mut sink = log_file.try_clone().map_err(|e| format!("clone: {e}"))?;
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = writeln!(sink, "[out] {line}");
            }
        });
    }
    if let Some(pipe) = child.stderr.take() {
        let mut sink = log_file;
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = writeln!(sink, "[err] {line}");
            }
        });
    }

    Ok(child)
}

/// Build the CLI argv for a bridge profile. Must stay in sync with
/// bridge-desktop's spawn args in its main.rs.
fn bridge_cli_args(profile: &BridgeProfile) -> Vec<String> {
    let mut args: Vec<String> = vec!["--server".into(), profile.server.clone()];
    if !profile.token.is_empty() {
        args.push("--token".into());
        args.push(profile.token.clone());
    }
    if profile.browser {
        if profile.engine == "stealth" {
            args.push("--browser-engine".into());
            args.push("stealth".into());
        }
        if profile.headless {
            args.push("--headless".into());
        }
    } else {
        args.push("--no-browser".into());
    }
    if profile.shell {
        args.push("--shell".into());
    }
    if !profile.device.is_empty() {
        args.push("--device".into());
        args.push(profile.device.clone());
    }
    args
}



#[derive(Serialize, Clone, Debug)]
struct ComponentVersion {
    name: String,
    current: String,
    latest: String,
    update_available: bool,
}

#[derive(Serialize, Clone, Debug)]
struct VersionReport {
    components: Vec<ComponentVersion>,
    any_update: bool,
}

fn read_payload_version(app: &tauri::AppHandle, sub: &str, package: &str) -> String {
    let entry = match resource_payload_path(app, sub) {
        Ok(p) => p,
        Err(_) => return "unknown".to_string(),
    };
    let Some(root) = entry.parent() else { return "unknown".to_string() };
    let pkg_json = root.join("node_modules").join(package).join("package.json");
    let Ok(content) = std::fs::read_to_string(&pkg_json) else { return "unknown".to_string() };
    serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|v| v.get("version")?.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

async fn fetch_npm_latest(package: &str) -> String {
    // npm scoped packages: @scope/name → registry URL uses the raw path.
    // reqwest::Url::parse preserves @/slash in paths, so format! is fine.
    let url = format!("https://registry.npmjs.org/{package}/latest");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[update] fetch {url}: {e}");
            return "unknown".to_string();
        }
    };
    if !resp.status().is_success() {
        eprintln!("[update] fetch {url}: HTTP {}", resp.status());
        return "unknown".to_string();
    }
    resp.json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("version")?.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

async fn fetch_launcher_latest() -> String {
    let url = "https://api.github.com/repos/tianshu-ai/tianshu-launcher/releases/latest";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = match client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "tianshu-launcher")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[update] fetch launcher latest: {e}");
            return "unknown".to_string();
        }
    };
    if !resp.status().is_success() {
        eprintln!("[update] fetch launcher latest: HTTP {}", resp.status());
        return "unknown".to_string();
    }
    resp.json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("tag_name")?.as_str().map(|s| s.trim_start_matches('v').to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn is_update_available(current: &str, latest: &str) -> bool {
    if current == "unknown" || latest == "unknown" || current == latest { return false; }
    let parse = |v: &str| -> (Vec<u64>, bool) {
        let (base, pre) = v.split_once('-').map_or((v, ""), |(a, b)| (a, b));
        let nums = base.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect();
        (nums, !pre.is_empty())
    };
    let (cur_n, cur_pre) = parse(current);
    let (lat_n, lat_pre) = parse(latest);
    match lat_n.cmp(&cur_n) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => cur_pre && !lat_pre,
    }
}

#[tauri::command]
async fn check_updates(app: tauri::AppHandle) -> Result<VersionReport, String> {
    let launcher_current = env!("CARGO_PKG_VERSION").to_string();
    let tianshu_current = read_payload_version(&app, "server", "@tianshu-ai/tianshu");
    let bridge_current = read_payload_version(&app, "bridge", "@tianshu-ai/local-bridge");
    let launcher_latest = fetch_launcher_latest().await;
    let tianshu_latest = fetch_npm_latest("@tianshu-ai/tianshu").await;
    let bridge_latest = fetch_npm_latest("@tianshu-ai/local-bridge").await;
    let components = vec![
        ComponentVersion {
            name: "Launcher".to_string(),
            update_available: is_update_available(&launcher_current, &launcher_latest),
            current: launcher_current, latest: launcher_latest,
        },
        ComponentVersion {
            name: "Tianshu Server".to_string(),
            update_available: is_update_available(&tianshu_current, &tianshu_latest),
            current: tianshu_current, latest: tianshu_latest,
        },
        ComponentVersion {
            name: "Local Bridge".to_string(),
            update_available: is_update_available(&bridge_current, &bridge_latest),
            current: bridge_current, latest: bridge_latest,
        },
    ];
    let any_update = components.iter().any(|c| c.update_available);
    Ok(VersionReport { components, any_update })
}

#[tauri::command]
async fn update_payload(app: tauri::AppHandle, sub: String, package: String) -> Result<(), String> {
    if sub != "server" && sub != "bridge" {
        return Err(format!("unknown payload component: {sub}"));
    }
    let node = node_sidecar_path(&app)?;
    let npm_cli = bundled_npm_cli(&app)?;
    tauri::async_runtime::spawn_blocking(move || install_payload_override(&sub, &package, &node, &npm_cli))
        .await
        .map_err(|e| format!("update_payload join: {e}"))??;
    Ok(())
}

#[tauri::command]
async fn update_launcher(app: tauri::AppHandle) -> Result<String, String> {
    let updater = app.updater_builder().build().map_err(|e| format!("updater init: {e}"))?;
    let update = updater.check().await.map_err(|e| format!("check: {e}"))?;
    match update {
        Some(up) => {
            let version = up.version.clone();
            println!("[launcher] downloading update v{version}...");
            up.download_and_install(|_, _| {}, || {}).await.map_err(|e| format!("install: {e}"))?;
            println!("[launcher] update v{version} installed, restart required");
            Ok(format!("Updated to {version}. Restarting..."))
        }
        None => Ok("Already up to date".into()),
    }
}

/// Locate the bundled npm-cli.js shipped in resources/npm/.
fn bundled_npm_cli(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // Bundled: resources/npm/bin/npm-cli.js
    let res = strip_unc_prefix(app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?);
    let cli = res.join("resources").join("npm").join("bin").join("npm-cli.js");
    if cli.exists() {
        return Ok(cli);
    }
    // Dev mode: src-tauri/resources/npm/bin/npm-cli.js
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("npm")
        .join("bin")
        .join("npm-cli.js");
    if dev.exists() {
        return Ok(dev);
    }
    Err("bundled npm-cli.js not found in resources/npm/bin/".into())
}

/// Locate npm on the system. Tauri-packaged .exe doesn't inherit the
/// user's shell PATH, so `Command::new("npm")` fails on Windows where
/// npm lives in `C:\Program Files\nodejs\` or `%APPDATA%\npm` — neither
/// of which is on the process's PATH.
///
/// Strategy: try the bare name first (works when PATH is inherited,
/// e.g. `npm run dev`), then probe well-known locations.
#[allow(dead_code)] // Kept as fallback if bundled npm is unavailable.
fn find_npm() -> Result<PathBuf, String> {
    // 1. If npm is on PATH, use it (covers dev mode + Unix + nvm).
    let bare = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let mut where_cmd = Command::new(if cfg!(windows) { "where" } else { "which" });
    where_cmd.arg(bare);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        where_cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Ok(output) = where_cmd.output()
    {
        if output.status.success() {
            let found = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !found.is_empty() {
                return Ok(PathBuf::from(found));
            }
        }
    }

    // 2. Windows well-known paths.
    #[cfg(windows)]
    {
        let candidates: Vec<PathBuf> = [
            std::env::var("ProgramFiles").ok().map(|p| PathBuf::from(p).join("nodejs").join("npm.cmd")),
            std::env::var("APPDATA").ok().map(|p| PathBuf::from(p).join("npm").join("npm.cmd")),
            Some(PathBuf::from(r"C:\Program Files\nodejs\npm.cmd")),
        ]
        .into_iter()
        .flatten()
        .collect();
        for c in &candidates {
            if c.exists() {
                return Ok(c.clone());
            }
        }
    }

    // 3. macOS / Linux: nvm, homebrew, system.
    #[cfg(unix)]
    {
        let home = home_dir();
        let candidates = [
            home.join(".nvm/versions/node"),  // nvm: pick latest
            PathBuf::from("/opt/homebrew/bin/npm"),
            PathBuf::from("/usr/local/bin/npm"),
            PathBuf::from("/usr/bin/npm"),
        ];
        // nvm: find the newest installed version's npm
        if candidates[0].is_dir() {
            if let Ok(entries) = std::fs::read_dir(&candidates[0]) {
                let mut versions: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().join("bin/npm"))
                    .filter(|p| p.exists())
                    .collect();
                versions.sort();
                if let Some(latest) = versions.pop() {
                    return Ok(latest);
                }
            }
        }
        for c in &candidates[1..] {
            if c.exists() {
                return Ok(c.clone());
            }
        }
    }

    Err("npm not found: not on PATH and not in well-known locations. Install Node.js or add npm to PATH.".into())
}

fn install_payload_override(sub: &str, package: &str, node: &Path, npm_cli: &Path) -> Result<(), String> {
    let override_dir = payload_override_dir(sub);
    let tmp = override_dir.with_file_name(format!("{sub}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("mkdir tmp: {e}"))?;
    std::fs::write(tmp.join("package.json"), serde_json::json!({"name":"x","private":true}).to_string())
        .map_err(|e| format!("write package.json: {e}"))?;

    // Use the bundled node + npm-cli.js. This way the launcher doesn't
    // need a system npm at all — the node sidecar already ships a full
    // npm distribution in resources/npm/.
    // Also inject node's parent dir into PATH so any lifecycle scripts
    // that use `#!/usr/bin/env node` resolve correctly.
    let enriched_path = {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        match node.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => {
                let sep = if cfg!(windows) { ";" } else { ":" };
                format!("{}{sep}{sys_path}", dir.display())
            }
            _ => sys_path,
        }
    };
    let mut cmd = Command::new(node);
    cmd.arg(npm_cli)
        .args(["install", "--omit=dev", "--no-audit", "--no-fund", "--legacy-peer-deps", &format!("{package}@latest")])
        .current_dir(&tmp)
        .env("PATH", &enriched_path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output()
        .map_err(|e| format!("npm install via bundled node: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("npm install failed: {}", String::from_utf8_lossy(&out.stderr)));
    }

    let pkg_dir = tmp.join("node_modules").join(package);
    if !pkg_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("installed package not found at {pkg_dir:?}"));
    }

    let bin_rel = if sub == "server" {
        pkg_dir.join("packages/server/dist/index.js")
    } else {
        let pkg_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(pkg_dir.join("package.json")).unwrap_or_default()
        ).unwrap_or(serde_json::Value::Null);
        let bin = pkg_json.get("bin").and_then(|b| match b {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(m) => m.values().next().and_then(|v| v.as_str().map(|s| s.to_string())),
            _ => None,
        }).unwrap_or_else(|| "dist/index.js".to_string());
        pkg_dir.join(&bin)
    };
    if !bin_rel.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("entry not found: {bin_rel:?}"));
    }

    let rel = bin_rel.strip_prefix(&tmp).map_err(|e| format!("strip_prefix: {e}"))?;
    let shim = format!("import \"./{}\";\n", rel.to_string_lossy().replace('\\', "/"));
    std::fs::write(tmp.join("index.js"), shim).map_err(|e| format!("write shim: {e}"))?;
    std::fs::write(
        tmp.join("package.json"),
        serde_json::json!({"name": format!("tianshu-{sub}-override"), "private": true, "type": "module"}).to_string(),
    ).map_err(|e| format!("write override package.json: {e}"))?;

    if override_dir.exists() {
        let backup = override_dir.with_file_name(format!("{sub}.old.{}", std::process::id()));
        std::fs::rename(&override_dir, &backup).map_err(|e| format!("backup rename: {e}"))?;
        if let Err(e) = std::fs::rename(&tmp, &override_dir) {
            let _ = std::fs::rename(&backup, &override_dir);
            return Err(format!("swap rename: {e}"));
        }
        let _ = std::fs::remove_dir_all(&backup);
    } else {
        if let Some(parent) = override_dir.parent() { let _ = std::fs::create_dir_all(parent); }
        std::fs::rename(&tmp, &override_dir).map_err(|e| format!("swap rename: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
fn restart_launcher(app: tauri::AppHandle, state: State<ProcState>) {
    // Mirror the tray's Quit handler: kill server + every bridge child
    // before exiting so a stale shim doesn't survive to squat on 3110.
    {
        let mut server = state.server.lock().unwrap();
        kill_child(&mut server);
    }
    {
        let mut bridges = state.bridges.lock().unwrap();
        for (_, mut child) in bridges.drain() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    // Spawn a new instance of ourselves before exiting. The child
    // inherits no stdio (detached) so it outlives this process.
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = Command::new(&exe);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| { libc::setsid(); Ok(()) });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
            const DETACHED_PROCESS: u32 = 0x00000008;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
        }
        match cmd.spawn() {
            Ok(_) => println!("[launcher] spawned new instance for restart"),
            Err(e) => eprintln!("[launcher] failed to spawn restart: {e}"),
        }
    }
    // Brief delay so the child has time to start before we exit.
    std::thread::sleep(std::time::Duration::from_millis(500));
    app.exit(0);
}


// ─── Tauri commands (invoked from UI) ───────────────────────────────

#[tauri::command]
fn status(state: State<ProcState>) -> Status {
    status_inner(&state)
}

#[tauri::command]
async fn start_server(app: tauri::AppHandle, state: State<'_, ProcState>) -> Result<Status, String> {
    {
        let server = state.server.lock().unwrap();
        if server.is_some() {
            drop(server);
            return Ok(status_inner(&state));
        }
    }
    let node = node_sidecar_path(&app)?;
    let entry = resource_payload_path(&app, "server")?;
    let web = web_dist_path(&app)?;
    let ignore = PathBuf::from("1");
    let child = spawn_child_logged(
        &node,
        &entry,
        &[
            ("TIANSHU_WEB_DIST", web),
            ("TIANSHU_IGNORE_SETUP", ignore),
        ],
        "server",
        &[],
    )?;
    {
        let mut server = state.server.lock().unwrap();
        *server = Some(child);
    }
    // Emit early so UI can show "Starting…" immediately.
    let _ = app.emit("server-starting", ());
    refresh_tray(&app);

    // Wait for port 3110 to accept connections (up to 30s).
    let ready = wait_for_port(3110, std::time::Duration::from_secs(30)).await;
    if !ready {
        // Server process may have crashed — check if still alive.
        let mut server = state.server.lock().unwrap();
        if let Some(ref mut child) = *server {
            match child.try_wait() {
                Ok(Some(_)) => { *server = None; } // exited
                _ => {} // still running, port just slow
            }
        }
    }
    let _ = app.emit("status-changed", ());
    refresh_tray(&app);
    Ok(status_inner(&state))
}

/// Non-command status helper (avoids move issues with State).
fn status_inner(state: &ProcState) -> Status {
    let server = state.server.lock().unwrap();
    let bridges = state.bridges.lock().unwrap();
    Status {
        server_running: server.is_some(),
        bridge_running: !bridges.is_empty(),
        server_port: 3110,
    }
}

/// Poll a TCP port until it accepts a connection or timeout expires.
async fn wait_for_port(port: u16, timeout: std::time::Duration) -> bool {
    tauri::async_runtime::spawn_blocking(move || {
        let start = std::time::Instant::now();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        while start.elapsed() < timeout {
            match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)) {
                Ok(_) => return true,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

#[tauri::command]
fn stop_server(app: tauri::AppHandle, state: State<ProcState>) -> Result<Status, String> {
    {
        let mut server = state.server.lock().unwrap();
        kill_child(&mut server);
    }
    let _ = app.emit("status-changed", ());
    refresh_tray(&app);
    Ok(status(state))
}

// ─── bridge profile commands ────────────────────────────────────────

#[derive(Serialize)]
struct ProfileStatus {
    id: String,
    name: String,
    server: String,
    running: bool,
}

#[tauri::command]
fn load_bridge_profiles() -> BridgeConfig {
    load_bridge_config()
}

#[tauri::command]
fn save_bridge_profiles(cfg: BridgeConfig) -> Result<(), String> {
    save_bridge_config(&cfg)
}

#[tauri::command]
fn bridge_status(state: State<ProcState>) -> Vec<ProfileStatus> {
    let cfg = load_bridge_config();
    let mut bridges = state.bridges.lock().unwrap();
    // Reap dead children before reporting.
    let dead: Vec<String> = bridges
        .iter_mut()
        .filter_map(|(id, child)| match child.try_wait() {
            Ok(Some(_)) => Some(id.clone()),
            _ => None,
        })
        .collect();
    for id in &dead {
        bridges.remove(id);
    }
    cfg.profiles
        .iter()
        .map(|p| ProfileStatus {
            id: p.id.clone(),
            name: p.name.clone(),
            server: p.server.clone(),
            running: bridges.contains_key(&p.id),
        })
        .collect()
}

#[tauri::command]
fn start_bridge_profile(
    id: String,
    app: tauri::AppHandle,
    state: State<ProcState>,
) -> Result<(), String> {
    let cfg = load_bridge_config();
    let profile = cfg
        .profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("profile not found: {id}"))?
        .clone();
    let node = node_sidecar_path(&app)?;
    let entry = resource_payload_path(&app, "bridge")?;
    let args = bridge_cli_args(&profile);
    // Sanitise profile name for a filename: 'My WSS!' -> 'my-wss'.
    let safe = profile
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>();
    let label = format!("bridge-{safe}");
    let child = spawn_child_logged(&node, &entry, &[], &label, &args)?;
    {
        let mut bridges = state.bridges.lock().unwrap();
        if let Some(mut old) = bridges.remove(&id) {
            let _ = old.kill();
            let _ = old.wait();
        }
        bridges.insert(id, child);
    }
    let _ = app.emit("status-changed", ());
    refresh_tray(&app);
    Ok(())
}

#[tauri::command]
fn stop_bridge_profile(
    id: String,
    app: tauri::AppHandle,
    state: State<ProcState>,
) -> Result<(), String> {
    {
        let mut bridges = state.bridges.lock().unwrap();
        if let Some(mut child) = bridges.remove(&id) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    let _ = app.emit("status-changed", ());
    refresh_tray(&app);
    Ok(())
}

#[tauri::command]
fn open_web_ui(app: tauri::AppHandle, state: State<ProcState>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let port = status(state).server_port;
    app.opener()
        .open_url(format!("http://localhost:{port}"), None::<&str>)
        .map_err(|e| format!("open_url: {e}"))
}

// ─── main ───────────────────────────────────────────────────────────

/// Windows-only: a process-wide Job Object configured so every
/// process assigned to it is killed when the launcher exits (even
/// via taskmgr /F). Created once at startup; every spawned sidecar
/// gets added via AssignProcessToJobObject.
///
/// We store the HANDLE as a usize in an AtomicU64 so the whole thing
/// is Send+Sync without requiring a global Mutex<HANDLE>. HANDLE is
/// a pointer; usize is wide enough on both 32- and 64-bit Windows.
#[cfg(windows)]
static LAUNCHER_JOB: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

#[cfg(windows)]
fn ensure_job_object() -> Result<isize, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    if let Some(h) = LAUNCHER_JOB.get() {
        return Ok(*h);
    }
    unsafe {
        let job: HANDLE = CreateJobObjectW(None, windows::core::PCWSTR::null())
            .map_err(|e| format!("CreateJobObjectW: {e}"))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        };
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject: {e}"))?;
        let raw = job.0 as isize;
        let _ = LAUNCHER_JOB.set(raw);
        Ok(raw)
    }
}

#[cfg(windows)]
fn assign_to_launcher_job(child: &Child) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::AssignProcessToJobObject;

    let job_raw = ensure_job_object()?;
    let job = HANDLE(job_raw as *mut _);
    let proc_handle = HANDLE(child.as_raw_handle() as *mut _);
    unsafe {
        AssignProcessToJobObject(job, proc_handle)
            .map_err(|e| format!("AssignProcessToJobObject: {e}"))?;
    }
    Ok(())
}

fn main() {
    // Windows: create the kill-on-close Job Object before any sidecar
    // can spawn. Failure here is non-fatal — we fall back to taskkill
    // /T on shutdown, which is less reliable (misses children spawned
    // after Rust loses track) but still catches the common case.
    #[cfg(windows)]
    {
        if let Err(e) = ensure_job_object() {
            eprintln!("[warn] launcher Job Object init failed: {e}");
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(ProcState::default())
        .manage(TrayItems::default())
        .setup(|app| {
            // Grouped tray menu. Tauri 2 doesn't expose NSMenuItem
            // image/icon APIs cross-platform, so emoji glyphs in the
            // label are the only consistent visual cue we have.

            // Header: brand + version, disabled so it reads as a title.
            let header = MenuItem::with_id(
                app,
                "header",
                format!("Tianshu  v{}", env!("CARGO_PKG_VERSION")),
                false,
                None::<&str>,
            )?;
            let sep1 = PredefinedMenuItem::separator(app)?;

            // Primary actions.
            let open_settings =
                MenuItem::with_id(app, "open_settings", "Settings\u{2026}", true, None::<&str>)?;
            let open_ui =
                MenuItem::with_id(app, "open_ui", "Open Web UI", true, None::<&str>)?;
            let open_config =
                MenuItem::with_id(app, "open_config", "Open Config Folder", true, None::<&str>)?;
            let sep2 = PredefinedMenuItem::separator(app)?;

            // Server submenu.
            let toggle_server =
                MenuItem::with_id(app, "toggle_server", "Start Server", true, None::<&str>)?;
            let restart_server =
                MenuItem::with_id(app, "restart_server", "Restart Server", true, None::<&str>)?;
            let server_submenu = Submenu::with_id_and_items(
                app,
                "server_submenu",
                "Server",
                true,
                &[&toggle_server, &restart_server],
            )?;

            // Bridge submenu.
            let manage_bridge = MenuItem::with_id(
                app,
                "manage_bridge",
                "Manage Profiles\u{2026}",
                true,
                None::<&str>,
            )?;
            let start_all_bridges = MenuItem::with_id(
                app,
                "start_all_bridges",
                "Start All Bridges",
                true,
                None::<&str>,
            )?;
            let stop_all_bridges = MenuItem::with_id(
                app,
                "stop_all_bridges",
                "Stop All Bridges",
                true,
                None::<&str>,
            )?;
            let bridge_submenu = Submenu::with_id_and_items(
                app,
                "bridge_submenu",
                "Local Bridge",
                true,
                &[&manage_bridge, &start_all_bridges, &stop_all_bridges],
            )?;
            // Keep the submenu handle for dynamic per-profile rebuilds.
            // The initial content (manage_bridge + stop_all_bridges)
            // will be cleared on first refresh_tray — that's fine, we
            // re-append fresh handles there.

            let sep3 = PredefinedMenuItem::separator(app)?;
            let quit =
                MenuItem::with_id(app, "quit", "Quit Tianshu", true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &header,
                    &sep1,
                    &open_settings,
                    &open_ui,
                    &open_config,
                    &sep2,
                    &server_submenu,
                    &bridge_submenu,
                    &sep3,
                    &quit,
                ],
            )?;

            // Stash handles for the items whose labels/enabled flag
            // change at runtime. refresh_tray looks them up via
            // app.state::<TrayItems>().
            let items: State<TrayItems> = app.state();
            *items.toggle_server.lock().unwrap() = Some(toggle_server.clone());
            *items.restart_server.lock().unwrap() = Some(restart_server.clone());
            *items.start_all_bridges.lock().unwrap() = Some(start_all_bridges.clone());
            *items.stop_all_bridges.lock().unwrap() = Some(stop_all_bridges.clone());
            *items.bridge_submenu.lock().unwrap() = Some(bridge_submenu.clone());
            // Initial state: no server, no bridges — reflect that.
            let _ = restart_server.set_enabled(false);
            let _ = start_all_bridges.set_enabled(false);
            let _ = stop_all_bridges.set_enabled(false);
            // Populate per-profile items from config on first launch.
            let profiles = load_bridge_config().profiles;
            rebuild_bridge_submenu(app.handle(), &items, &profiles);
            *items.bridge_profile_signature.lock().unwrap() =
                profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect();

            let icon = tauri::image::Image::from_bytes(ICON_STOPPED)?;
            let _tray = TrayIconBuilder::with_id("main")
                .icon(icon)
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "quit" => {
                        let state: State<ProcState> = app.state();
                        {
                            let mut server = state.server.lock().unwrap();
                            kill_child(&mut server);
                        }
                        {
                            let mut bridges = state.bridges.lock().unwrap();
                            for (_, child) in bridges.iter_mut() {
                                kill_child_ref(child);
                            }
                            bridges.clear();
                        }
                        app.exit(0);
                    }
                    "open_settings" | "manage_bridge" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "open_ui" => {
                        use tauri_plugin_opener::OpenerExt;
                        let _ = app
                            .opener()
                            .open_url("http://localhost:3110", None::<&str>);
                    }
                    "open_config" => {
                        use tauri_plugin_opener::OpenerExt;
                        let tianshu_dir = home_dir().join(".tianshu");
                        // Create it if missing so the file manager
                        // doesn't bail with 'folder does not exist'.
                        let _ = std::fs::create_dir_all(&tianshu_dir);
                        let _ = app
                            .opener()
                            .open_path(tianshu_dir.to_string_lossy().to_string(), None::<&str>);
                    }
                    "toggle_server" => {
                        let state: State<ProcState> = app.state();
                        let running = state.server.lock().unwrap().is_some();
                        if running {
                            let _ = stop_server(app.clone(), state);
                        } else {
                            let _ = start_server(app.clone(), state);
                        }
                    }
                    "restart_server" => {
                        let state: State<ProcState> = app.state();
                        let _ = stop_server(app.clone(), state);
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        let state: State<ProcState> = app.state();
                        let _ = start_server(app.clone(), state);
                    }
                    "start_all_bridges" => {
                        let profiles = load_bridge_config().profiles;
                        let state: State<ProcState> = app.state();
                        let running: std::collections::HashSet<String> = state
                            .bridges
                            .lock()
                            .unwrap()
                            .keys()
                            .cloned()
                            .collect();
                        for p in profiles {
                            if running.contains(&p.id) {
                                continue;
                            }
                            let state: State<ProcState> = app.state();
                            let _ = start_bridge_profile(p.id, app.clone(), state);
                        }
                    }
                    "stop_all_bridges" => {
                        let state: State<ProcState> = app.state();
                        let ids: Vec<String> = state
                            .bridges
                            .lock()
                            .unwrap()
                            .keys()
                            .cloned()
                            .collect();
                        for id in ids {
                            let state: State<ProcState> = app.state();
                            let _ = stop_bridge_profile(id, app.clone(), state);
                        }
                    }
                    other if other.starts_with("bridge_toggle_") => {
                        let profile_id = other.trim_start_matches("bridge_toggle_").to_string();
                        let state: State<ProcState> = app.state();
                        let running = state
                            .bridges
                            .lock()
                            .unwrap()
                            .contains_key(&profile_id);
                        if running {
                            let state: State<ProcState> = app.state();
                            let _ = stop_bridge_profile(profile_id, app.clone(), state);
                        } else {
                            let state: State<ProcState> = app.state();
                            let _ = start_bridge_profile(profile_id, app.clone(), state);
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            // Auto-start the server on first launch, plus any bridge
            // profile that has auto_start=true. User can stop any of
            // them from the tray or UI if they don't want them running.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state: State<ProcState> = app_handle.state();
                let _ = start_server(app_handle.clone(), state);

                // Give the server a moment to bind :3110 before auto-
                // starting any bridge profile that points at it (the
                // common 'localhost' case). Pure best-effort — the
                // bridge CLI reconnects on failure anyway.
                std::thread::sleep(std::time::Duration::from_millis(600));
                let profiles = load_bridge_config().profiles;
                for p in profiles {
                    if !p.auto_start {
                        continue;
                    }
                    let state: State<ProcState> = app_handle.state();
                    let _ = start_bridge_profile(p.id, app_handle.clone(), state);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            status,
            start_server,
            stop_server,
            open_web_ui,
            load_bridge_profiles,
            save_bridge_profiles,
            bridge_status,
            start_bridge_profile,
            stop_bridge_profile,
            check_updates,
            update_payload,
            update_launcher,
            restart_launcher
        ])
        .on_window_event(|window, event| {
            // Hide window on close instead of quitting; tray stays alive.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tianshu-launcher")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Kill every sidecar we spawned. Without this, setsid
                // (Unix) / CREATE_NEW_PROCESS_GROUP (Windows) means
                // Node sidecars survive the launcher's exit and keep
                // holding port 3110 forever.
                let state: State<ProcState> = app.state();
                {
                    let mut server = state.server.lock().unwrap();
                    kill_child(&mut server);
                }
                {
                    let mut bridges = state.bridges.lock().unwrap();
                    for (_, child) in bridges.iter_mut() {
                        kill_child_ref(child);
                    }
                    bridges.clear();
                }
            }
        });
}


