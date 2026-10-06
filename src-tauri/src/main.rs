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
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};

/// Update the tray icon to reflect current running state. Synchronous
/// — called from inside each start/stop command after it releases its
/// own state locks. Deliberately NOT wired through app.listen() to
/// avoid any chance of the event loop + lock-reacquire deadlocking
/// the webview.
fn refresh_tray(app: &tauri::AppHandle) {
    let state: State<ProcState> = app.state();
    let server_running = state.server.lock().unwrap().is_some();
    let any_bridge = !state.bridges.lock().unwrap().is_empty();
    let active = server_running || any_bridge;
    if let Some(tray) = app.tray_by_id("main") {
        let bytes: &[u8] = if active { ICON_RUNNING } else { ICON_STOPPED };
        if let Ok(img) = tauri::image::Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(img));
        }
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

fn bridge_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("USERPROFILE") {
            return PathBuf::from(p).join(".tianshu-bridge");
        }
    }
    if let Ok(p) = std::env::var("HOME") {
        return PathBuf::from(p).join(".tianshu-bridge");
    }
    PathBuf::from(".tianshu-bridge")
}

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
    let name = if cfg!(windows) {
        format!("node-{triple}.exe")
    } else {
        format!("node-{triple}")
    };
    // Production: Tauri unpacks externalBin into the resource dir.
    let base = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?;
    let candidate = base.join(&name);
    if candidate.exists() {
        return Ok(candidate);
    }
    let nested = base.join("binaries").join(&name);
    if nested.exists() {
        return Ok(nested);
    }
    // Dev-mode fallback: externalBin isn't copied to target/debug.
    // Walk up from the current exe (target/debug/<app>) to find
    // src-tauri/binaries/<name>.
    if let Ok(exe) = std::env::current_exe() {
        // exe = .../src-tauri/target/debug/<app>
        //                           ^^^ parent = debug
        //                     ^^^ parent.parent = target
        //              ^^^ parent.parent.parent = src-tauri
        if let Some(src_tauri) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let dev = src_tauri.join("binaries").join(&name);
            if dev.exists() {
                return Ok(dev);
            }
        }
    }
    Err(format!(
        "node sidecar not found: tried {candidate:?}, {nested:?}, and dev-mode src-tauri/binaries/{name}"
    ))
}

/// Where the bundled payload (server + bridge) lives.
fn resource_payload_path(app: &tauri::AppHandle, sub: &str) -> Result<PathBuf, String> {
    let base = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?;
    let candidate = base.join("resources").join(sub).join("index.js");
    if candidate.exists() {
        return Ok(candidate);
    }
    // Dev-mode fallback: resources/ isn't copied to target/debug.
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

fn spawn_child(
    node: &PathBuf,
    entry: &PathBuf,
    extra_env: &[(&str, PathBuf)],
) -> Result<Child, String> {
    let mut cmd = Command::new(node);
    cmd.arg(entry);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        // Hide console window on Windows.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map_err(|e| format!("spawn failed: {e}"))
}

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

fn kill_child(child_opt: &mut Option<Child>) {
    if let Some(mut child) = child_opt.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
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

fn spawn_bridge_child(
    node: &PathBuf,
    entry: &PathBuf,
    profile: &BridgeProfile,
) -> Result<Child, String> {
    let mut cmd = Command::new(node);
    cmd.arg(entry);
    cmd.args(bridge_cli_args(profile));
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map_err(|e| format!("bridge spawn failed: {e}"))
}

// ─── Tauri commands (invoked from UI) ───────────────────────────────

#[tauri::command]
fn status(state: State<ProcState>) -> Status {
    let server = state.server.lock().unwrap();
    let bridges = state.bridges.lock().unwrap();
    Status {
        server_running: server.is_some(),
        bridge_running: !bridges.is_empty(),
        server_port: 3110, // tianshu default; could be read from config.json later
    }
}

#[tauri::command]
fn start_server(app: tauri::AppHandle, state: State<ProcState>) -> Result<Status, String> {
    let mut server = state.server.lock().unwrap();
    if server.is_some() {
        drop(server);
        return Ok(status(state));
    }
    let node = node_sidecar_path(&app)?;
    let entry = resource_payload_path(&app, "server")?;
    let web = web_dist_path(&app)?;
    let child = spawn_child(&node, &entry, &[("TIANSHU_WEB_DIST", web)])?;
    *server = Some(child);
    drop(server);
    let _ = app.emit("status-changed", ());
    refresh_tray(&app);
    Ok(status(state))
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
    let child = spawn_bridge_child(&node, &entry, &profile)?;
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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(ProcState::default())
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
            let open_settings = MenuItem::with_id(
                app,
                "open_settings",
                "\u{2699}  Settings\u{2026}",
                true,
                None::<&str>,
            )?;
            let open_ui = MenuItem::with_id(
                app,
                "open_ui",
                "\u{1f310}  Open Tianshu Web UI",
                true,
                None::<&str>,
            )?;
            let open_config = MenuItem::with_id(
                app,
                "open_config",
                "\u{1f4c2}  Open Config Folder",
                true,
                None::<&str>,
            )?;
            let sep2 = PredefinedMenuItem::separator(app)?;

            // Server submenu.
            let toggle_server = MenuItem::with_id(
                app,
                "toggle_server",
                "\u{25b6}  Start Server",
                true,
                None::<&str>,
            )?;
            let restart_server = MenuItem::with_id(
                app,
                "restart_server",
                "\u{1f501}  Restart Server",
                true,
                None::<&str>,
            )?;
            let server_submenu = Submenu::with_id_and_items(
                app,
                "server_submenu",
                "\u{1f5a5}\u{fe0f}  Server",
                true,
                &[&toggle_server, &restart_server],
            )?;

            // Bridge submenu.
            let manage_bridge = MenuItem::with_id(
                app,
                "manage_bridge",
                "\u{2699}  Manage Profiles\u{2026}",
                true,
                None::<&str>,
            )?;
            let stop_all_bridges = MenuItem::with_id(
                app,
                "stop_all_bridges",
                "\u{23f9}  Stop All Bridges",
                true,
                None::<&str>,
            )?;
            let bridge_submenu = Submenu::with_id_and_items(
                app,
                "bridge_submenu",
                "\u{1f309}  Local Bridge",
                true,
                &[&manage_bridge, &stop_all_bridges],
            )?;

            let sep3 = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(
                app,
                "quit",
                "\u{23fb}  Quit Tianshu",
                true,
                None::<&str>,
            )?;

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
                            for (_, mut child) in bridges.drain() {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
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
                        let home = std::env::var("HOME").unwrap_or_default();
                        let _ = app
                            .opener()
                            .open_path(format!("{home}/.tianshu"), None::<&str>);
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
                    _ => {}
                })
                .build(app)?;

            // Auto-start the server on first launch. User can stop it
            // from the tray or UI if they don't want it.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state: State<ProcState> = app_handle.state();
                let _ = start_server(app_handle.clone(), state);
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
            stop_bridge_profile
        ])
        .on_window_event(|window, event| {
            // Hide window on close instead of quitting; tray stays alive.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tianshu-launcher");
}


