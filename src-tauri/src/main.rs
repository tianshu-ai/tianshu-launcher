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

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};

static ICON_STOPPED: &[u8] = include_bytes!("../icons/tray/stopped.png");
static ICON_RUNNING: &[u8] = include_bytes!("../icons/tray/running.png");

// ─── child-process state ────────────────────────────────────────────

#[derive(Default)]
struct ProcState {
    server: Mutex<Option<Child>>,
    bridge: Mutex<Option<Child>>,
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

fn spawn_child(node: &PathBuf, entry: &PathBuf) -> Result<Child, String> {
    let mut cmd = Command::new(node);
    cmd.arg(entry);
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

fn kill_child(child_opt: &mut Option<Child>) {
    if let Some(mut child) = child_opt.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ─── Tauri commands (invoked from UI) ───────────────────────────────

#[tauri::command]
fn status(state: State<ProcState>) -> Status {
    let server = state.server.lock().unwrap();
    let bridge = state.bridge.lock().unwrap();
    Status {
        server_running: server.is_some(),
        bridge_running: bridge.is_some(),
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
    let child = spawn_child(&node, &entry)?;
    *server = Some(child);
    let _ = app.emit("status-changed", ());
    drop(server);
    Ok(status(state))
}

#[tauri::command]
fn stop_server(app: tauri::AppHandle, state: State<ProcState>) -> Result<Status, String> {
    {
        let mut server = state.server.lock().unwrap();
        kill_child(&mut server);
    }
    let _ = app.emit("status-changed", ());
    Ok(status(state))
}

#[tauri::command]
fn start_bridge(app: tauri::AppHandle, state: State<ProcState>) -> Result<Status, String> {
    let mut bridge = state.bridge.lock().unwrap();
    if bridge.is_some() {
        drop(bridge);
        return Ok(status(state));
    }
    let node = node_sidecar_path(&app)?;
    let entry = resource_payload_path(&app, "bridge")?;
    let child = spawn_child(&node, &entry)?;
    *bridge = Some(child);
    let _ = app.emit("status-changed", ());
    drop(bridge);
    Ok(status(state))
}

#[tauri::command]
fn stop_bridge(app: tauri::AppHandle, state: State<ProcState>) -> Result<Status, String> {
    {
        let mut bridge = state.bridge.lock().unwrap();
        kill_child(&mut bridge);
    }
    let _ = app.emit("status-changed", ());
    Ok(status(state))
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
        .manage(ProcState::default())
        .setup(|app| {
            // Minimal tray: Open UI / Toggle Server / Toggle Bridge / Quit.
            let open_ui = MenuItem::with_id(app, "open_ui", "Open Tianshu", true, None::<&str>)?;
            let toggle_server =
                MenuItem::with_id(app, "toggle_server", "Start Server", true, None::<&str>)?;
            let toggle_bridge =
                MenuItem::with_id(app, "toggle_bridge", "Start Local Bridge", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[&open_ui, &toggle_server, &toggle_bridge, &sep, &quit],
            )?;

            let icon = tauri::image::Image::from_bytes(ICON_STOPPED)?;
            let _tray = TrayIconBuilder::new()
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
                            let mut bridge = state.bridge.lock().unwrap();
                            kill_child(&mut bridge);
                        }
                        app.exit(0);
                    }
                    "open_ui" => {
                        use tauri_plugin_opener::OpenerExt;
                        let _ = app
                            .opener()
                            .open_url("http://localhost:3110", None::<&str>);
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
                    "toggle_bridge" => {
                        let state: State<ProcState> = app.state();
                        let running = state.bridge.lock().unwrap().is_some();
                        if running {
                            let _ = stop_bridge(app.clone(), state);
                        } else {
                            let _ = start_bridge(app.clone(), state);
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
            start_bridge,
            stop_bridge,
            open_web_ui
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

// Suppress unused warning for ICON_RUNNING until pulsing animation is wired.
#[allow(dead_code)]
fn _icon_running() -> &'static [u8] {
    ICON_RUNNING
}
