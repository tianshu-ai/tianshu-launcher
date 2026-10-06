// Tianshu Launcher UI logic.
//
// withGlobalTauri=true in tauri.conf.json injects window.__TAURI__ with
// the core + event APIs. We don't use JS modules here (Tauri's custom
// protocol can be finicky with module MIME types on some platforms —
// a plain script tag is the lowest-friction path).

(function () {
  const tauri = window.__TAURI__;
  if (!tauri) {
    document.body.innerHTML =
      '<div style="padding:24px;color:#f87171;font-family:sans-serif">' +
      '<b>Tauri bridge not loaded.</b><br>Check withGlobalTauri in tauri.conf.json.' +
      "</div>";
    return;
  }
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;

  const els = {
    serverDot: document.getElementById("server-dot"),
    serverSub: document.getElementById("server-sub"),
    serverBtn: document.getElementById("server-btn"),
    bridgeDot: document.getElementById("bridge-dot"),
    bridgeBtn: document.getElementById("bridge-btn"),
    openUiBtn: document.getElementById("open-ui-btn"),
  };

  let state = { server_running: false, bridge_running: false, server_port: 3110 };

  function render() {
    els.serverDot.classList.toggle("on", state.server_running);
    els.serverSub.textContent = state.server_running
      ? "Running on port " + state.server_port
      : "Stopped";
    els.serverBtn.textContent = state.server_running ? "Stop" : "Start";
    els.serverBtn.classList.toggle("primary", !state.server_running);
    els.bridgeDot.classList.toggle("on", state.bridge_running);
    els.bridgeBtn.textContent = state.bridge_running ? "Stop" : "Start";
    els.bridgeBtn.classList.toggle("primary", !state.bridge_running);
    els.openUiBtn.disabled = !state.server_running;
  }

  async function refresh() {
    try {
      state = await invoke("status");
      render();
    } catch (err) {
      console.error("status failed:", err);
    }
  }

  els.serverBtn.addEventListener("click", async () => {
    els.serverBtn.disabled = true;
    try {
      state = await invoke(state.server_running ? "stop_server" : "start_server");
    } catch (err) {
      alert("Server error: " + err);
      console.error("start_server failed:", err);
    } finally {
      els.serverBtn.disabled = false;
      render();
    }
  });

  els.bridgeBtn.addEventListener("click", async () => {
    els.bridgeBtn.disabled = true;
    try {
      state = await invoke(state.bridge_running ? "stop_bridge" : "start_bridge");
    } catch (err) {
      alert("Bridge error: " + err);
      console.error("start_bridge failed:", err);
    } finally {
      els.bridgeBtn.disabled = false;
      render();
    }
  });

  els.openUiBtn.addEventListener("click", async () => {
    try {
      await invoke("open_web_ui");
    } catch (err) {
      alert("Open error: " + err);
    }
  });

  listen("status-changed", refresh);
  refresh();
  console.log("[tianshu-launcher] UI ready");
})();
