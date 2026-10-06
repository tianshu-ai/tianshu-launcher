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

  // ─── toast ──────────────────────────────────────────────────────

  const toastEl = document.getElementById("toast");
  let toastTimer = null;
  function toast(msg, kind) {
    toastEl.textContent = msg;
    toastEl.className = "toast visible" + (kind === "error" ? " error" : "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      toastEl.className = "toast" + (kind === "error" ? " error" : "");
    }, kind === "error" ? 4000 : 2000);
  }

  // ─── tab switching ──────────────────────────────────────────────

  document.querySelectorAll(".tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      const name = tab.dataset.tab;
      document.querySelectorAll(".tab").forEach((t) =>
        t.classList.toggle("active", t === tab),
      );
      document.querySelectorAll(".tab-panel").forEach((p) =>
        p.classList.toggle("active", p.id === "tab-" + name),
      );
    });
  });

  // ─── server tab ─────────────────────────────────────────────────

  const serverEls = {
    dot: document.getElementById("server-dot"),
    sub: document.getElementById("server-sub"),
    btn: document.getElementById("server-btn"),
    openBtn: document.getElementById("open-ui-btn"),
  };
  let serverState = { server_running: false, bridge_running: false, server_port: 3110 };

  function renderServer() {
    serverEls.dot.classList.toggle("on", serverState.server_running);
    serverEls.sub.textContent = serverState.server_running
      ? "Running on port " + serverState.server_port
      : "Stopped";
    serverEls.btn.textContent = serverState.server_running ? "Stop" : "Start";
    serverEls.btn.classList.toggle("primary", !serverState.server_running);
    serverEls.openBtn.disabled = !serverState.server_running;
  }

  async function refreshServer() {
    try {
      serverState = await invoke("status");
      renderServer();
    } catch (err) {
      console.error("status failed:", err);
    }
  }

  serverEls.btn.addEventListener("click", async () => {
    serverEls.btn.disabled = true;
    try {
      serverState = await invoke(serverState.server_running ? "stop_server" : "start_server");
    } catch (err) {
      toast("Server: " + err, "error");
      console.error(err);
    } finally {
      serverEls.btn.disabled = false;
      renderServer();
    }
  });

  serverEls.openBtn.addEventListener("click", async () => {
    try { await invoke("open_web_ui"); }
    catch (err) { toast("Open: " + err, "error"); }
  });

  // ─── bridge tab ─────────────────────────────────────────────────
  //
  // Profile list is derived from ~/.tianshu-bridge/config.json via two
  // Rust commands:
  //   - load_bridge_profiles / save_bridge_profiles — read/write JSON
  //   - bridge_status — list with current running flags
  //   - start_bridge_profile / stop_bridge_profile — per-profile toggle
  //
  // Each card can be expanded in-place to edit; "Save" writes the
  // whole config.json back.

  const listEl = document.getElementById("profile-list");
  const badgeEl = document.getElementById("bridge-badge");
  const addBtn = document.getElementById("add-profile-btn");
  const pasteBtn = document.getElementById("paste-profile-btn");
  // Tauri 2 clipboard plugin — may be undefined if plugin isn't loaded,
  // in which case we fall back to navigator.clipboard (requires focus).
  const readClipboard = tauri?.clipboardManager?.readText;
  // Edit-state cache so toggling edit mode doesn't reset in-progress
  // field changes while bridge_status polls every few seconds.
  const openEdits = new Set(); // profile ids currently being edited
  const editDrafts = new Map(); // id → draft BridgeProfile

  let profiles = [];
  let statusMap = new Map(); // id → running bool

  function updateBadge() {
    const running = Array.from(statusMap.values()).filter(Boolean).length;
    badgeEl.textContent = String(running);
    badgeEl.style.display = profiles.length === 0 ? "none" : "";
  }

  function renderBridge() {
    updateBadge();
    if (profiles.length === 0) {
      listEl.innerHTML =
        '<div class="empty">No bridge profiles yet.<br>Add one to connect this device to a Tianshu server.</div>';
      return;
    }
    listEl.innerHTML = "";
    profiles.forEach((p) => listEl.appendChild(renderProfileCard(p)));
  }

  function renderProfileCard(p) {
    const running = !!statusMap.get(p.id);
    const editing = openEdits.has(p.id);
    const draft = editDrafts.get(p.id) || { ...p };

    const card = document.createElement("div");
    card.className = "card profile-card" + (running ? " running" : "");

    // Head: name + status + buttons
    const head = document.createElement("div");
    head.className = "profile-head";

    const dot = document.createElement("span");
    dot.className = "dot" + (running ? " on" : "");
    head.appendChild(dot);

    const name = document.createElement("span");
    name.className = "profile-name";
    name.textContent = p.name || "(unnamed)";
    head.appendChild(name);

    const btnGroup = document.createElement("div");
    btnGroup.className = "btn-group";

    const toggleBtn = document.createElement("button");
    toggleBtn.textContent = running ? "Stop" : "Start";
    if (!running) toggleBtn.className = "primary";
    toggleBtn.addEventListener("click", async () => {
      toggleBtn.disabled = true;
      try {
        await invoke(running ? "stop_bridge_profile" : "start_bridge_profile", { id: p.id });
        await refreshBridge();
      } catch (err) {
        toast("Bridge: " + err, "error");
      } finally {
        toggleBtn.disabled = false;
      }
    });
    btnGroup.appendChild(toggleBtn);

    const editBtn = document.createElement("button");
    editBtn.textContent = editing ? "Close" : "Edit";
    editBtn.addEventListener("click", () => {
      if (openEdits.has(p.id)) {
        openEdits.delete(p.id);
        editDrafts.delete(p.id);
      } else {
        openEdits.add(p.id);
        editDrafts.set(p.id, { ...p });
      }
      renderBridge();
    });
    btnGroup.appendChild(editBtn);

    head.appendChild(btnGroup);
    card.appendChild(head);

    const serverLine = document.createElement("div");
    serverLine.className = "profile-server";
    serverLine.textContent = p.server;
    card.appendChild(serverLine);

    if (editing) card.appendChild(renderEditForm(p, draft));
    return card;
  }

  function renderEditForm(original, draft) {
    const form = document.createElement("div");
    form.className = "edit-form open";

    const text = (label, key, type = "text", placeholder = "") => {
      const row = document.createElement("div");
      row.className = "form-row";
      const l = document.createElement("label");
      l.textContent = label;
      row.appendChild(l);
      const input = document.createElement("input");
      input.type = type;
      input.value = draft[key] ?? "";
      input.placeholder = placeholder;
      input.autocomplete = "off";
      input.addEventListener("input", () => { draft[key] = input.value; });
      row.appendChild(input);
      return row;
    };

    const checkbox = (label, key) => {
      const row = document.createElement("label");
      row.className = "checkbox-row";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = !!draft[key];
      cb.addEventListener("change", () => { draft[key] = cb.checked; });
      row.appendChild(cb);
      const span = document.createElement("span");
      span.textContent = label;
      row.appendChild(span);
      return row;
    };

    form.appendChild(text("Name", "name"));
    form.appendChild(text("Server", "server", "text", "wss://tianshu.example.com/ws"));
    form.appendChild(text("Token", "token", "password", "optional auth token"));
    form.appendChild(text("Device", "device", "text", "device id (optional)"));

    // Engine select
    const engineRow = document.createElement("div");
    engineRow.className = "form-row";
    const eLabel = document.createElement("label");
    eLabel.textContent = "Browser";
    engineRow.appendChild(eLabel);
    const sel = document.createElement("select");
    ["own", "stealth"].forEach((v) => {
      const opt = document.createElement("option");
      opt.value = v;
      opt.textContent = v === "own" ? "own (bundled Chromium)" : "stealth (user profile)";
      if (draft.engine === v) opt.selected = true;
      sel.appendChild(opt);
    });
    sel.addEventListener("change", () => { draft.engine = sel.value; });
    engineRow.appendChild(sel);
    form.appendChild(engineRow);

    // Flags row
    const flagsRow = document.createElement("div");
    flagsRow.className = "form-row inline";
    const spacer = document.createElement("label");
    spacer.textContent = "";
    flagsRow.appendChild(spacer);
    flagsRow.appendChild(checkbox("Enable browser", "browser"));
    flagsRow.appendChild(checkbox("Headless", "headless"));
    flagsRow.appendChild(checkbox("Enable shell", "shell"));
    flagsRow.appendChild(checkbox("Auto-start", "auto_start"));
    form.appendChild(flagsRow);

    // Actions
    const actions = document.createElement("div");
    actions.className = "form-row actions";

    // Two-step delete: first click arms, second click within 3s confirms.
    // window.confirm() is blocked in Tauri's webview on some platforms,
    // so we handle confirmation inline.
    const delBtn = document.createElement("button");
    delBtn.className = "danger";
    delBtn.textContent = "Delete";
    let armed = false;
    let armTimer = null;
    delBtn.addEventListener("click", async () => {
      if (!armed) {
        armed = true;
        delBtn.textContent = "Click again to confirm";
        armTimer = setTimeout(() => {
          armed = false;
          delBtn.textContent = "Delete";
        }, 3000);
        return;
      }
      clearTimeout(armTimer);
      armed = false;
      try {
        if (statusMap.get(original.id)) {
          await invoke("stop_bridge_profile", { id: original.id });
        }
        profiles = profiles.filter((x) => x.id !== original.id);
        await invoke("save_bridge_profiles", { cfg: { profiles } });
        openEdits.delete(original.id);
        editDrafts.delete(original.id);
        await refreshBridge();
        toast("Deleted " + (original.name || "profile"));
      } catch (err) {
        toast("Delete: " + err, "error");
        console.error("delete failed:", err);
      }
    });
    actions.appendChild(delBtn);

    const spacer2 = document.createElement("div");
    spacer2.style.flex = "1";
    actions.appendChild(spacer2);

    const saveBtn = document.createElement("button");
    saveBtn.className = "primary";
    saveBtn.textContent = "Save";
    saveBtn.addEventListener("click", async () => {
      try {
        // Validate
        if (!draft.name?.trim()) {
          toast("Name required", "error");
          return;
        }
        if (!/^wss?:\/\//i.test(draft.server || "")) {
          toast("Server must start with ws:// or wss://", "error");
          return;
        }
        // Replace in list
        const idx = profiles.findIndex((x) => x.id === original.id);
        if (idx >= 0) profiles[idx] = { ...original, ...draft };
        await invoke("save_bridge_profiles", { cfg: { profiles } });
        openEdits.delete(original.id);
        editDrafts.delete(original.id);
        await refreshBridge();
        toast("Saved");
      } catch (err) {
        toast("Save: " + err, "error");
      }
    });
    actions.appendChild(saveBtn);

    form.appendChild(actions);
    return form;
  }

  async function refreshBridge() {
    try {
      const cfg = await invoke("load_bridge_profiles");
      profiles = cfg.profiles || [];
      const status = await invoke("bridge_status");
      statusMap = new Map(status.map((s) => [s.id, s.running]));
      renderBridge();
    } catch (err) {
      console.error("refreshBridge failed:", err);
    }
  }

  // Parse a Tianshu-bridge invite from clipboard. Accepted formats:
  //   1. tsbridge://configure?server=wss://...&token=***
  //   2. {"server":"wss://...","token":"..."}  (any BridgeProfile fields)
  //   3. wss://host/ws  or  wss://host/ws TOKEN
  //   4. tsbridge --server wss://host/ws --token TOKEN (full CLI command)
  function parseInvite(raw) {
    const trimmed = raw.trim();
    if (!trimmed) return null;
    // 1. tsbridge:// URL
    if (/^tsbridge:\/\//i.test(trimmed)) {
      try {
        const u = new URL(trimmed);
        return {
          server: u.searchParams.get("server") || "",
          token: u.searchParams.get("token") || "",
          device: u.searchParams.get("device") || "",
        };
      } catch { return null; }
    }
    // 2. JSON
    if (trimmed.startsWith("{")) {
      try { return JSON.parse(trimmed); } catch { return null; }
    }
    // 3. plain ws(s) url (with optional token after whitespace)
    if (/^wss?:\/\//i.test(trimmed)) {
      const parts = trimmed.split(/\s+/);
      return { server: parts[0], token: parts[1] || "" };
    }
    // 4. 'tsbridge --server ... --token ...' style command line
    if (/tsbridge\b|--server\b/.test(trimmed)) {
      // Split respecting single/double quotes.
      const tokens = [];
      const re = /"([^"]*)"|'([^']*)'|(\S+)/g;
      let m;
      while ((m = re.exec(trimmed)) !== null) {
        tokens.push(m[1] ?? m[2] ?? m[3]);
      }
      const result = {};
      const flagMap = { "--server": "server", "--token": "token", "--device": "device" };
      for (let i = 0; i < tokens.length; i++) {
        const t = tokens[i];
        if (flagMap[t] && tokens[i + 1]) {
          result[flagMap[t]] = tokens[i + 1];
          i++;
        } else if (t === "--no-browser") {
          result.browser = false;
        } else if (t === "--headless") {
          result.headless = true;
        } else if (t === "--shell") {
          result.shell = true;
        } else if (t === "--browser-engine" && tokens[i + 1]) {
          result.engine = tokens[i + 1];
          i++;
        }
      }
      return result.server ? result : null;
    }
    return null;
  }

  pasteBtn.addEventListener("click", async () => {
    let raw;
    try {
      raw = readClipboard
        ? await readClipboard()
        : await navigator.clipboard.readText();
    } catch (err) {
      toast("Clipboard unavailable: " + err, "error");
      return;
    }
    const parsed = parseInvite(raw || "");
    if (!parsed || !parsed.server) {
      toast("Couldn\u2019t parse \u2014 expected tsbridge:// URL, JSON, wss://, or tsbridge command", "error");
      return;
    }
    // Derive a friendly name from the hostname.
    let name;
    try { name = new URL(parsed.server).hostname; }
    catch { name = parsed.server.slice(0, 32); }
    const id = "p_" + Date.now().toString(16) + "_" + Math.floor(Math.random() * 0xffffffff).toString(16);
    const fresh = {
      id,
      name,
      server: parsed.server,
      token: parsed.token || "",
      device: parsed.device || "",
      auto_start: parsed.auto_start ?? true,
      browser: parsed.browser ?? true,
      engine: parsed.engine || "own",
      headless: parsed.headless || false,
      shell: parsed.shell || false,
    };
    profiles.push(fresh);
    try {
      await invoke("save_bridge_profiles", { cfg: { profiles } });
      await refreshBridge();
      toast("Added " + name + " from clipboard");
    } catch (err) {
      toast("Save: " + err, "error");
    }
  });

  addBtn.addEventListener("click", async () => {
    // Generate a client-side id matching Rust's gen_id format shape.
    const id = "p_" + Date.now().toString(16) + "_" + Math.floor(Math.random() * 0xffffffff).toString(16);
    const fresh = {
      id,
      name: "New profile",
      server: "ws://localhost:3110/ws",
      token: "",
      device: "",
      auto_start: true,
      browser: true,
      engine: "own",
      headless: false,
      shell: false,
    };
    profiles.push(fresh);
    try {
      await invoke("save_bridge_profiles", { cfg: { profiles } });
      openEdits.add(id);
      editDrafts.set(id, { ...fresh });
      await refreshBridge();
    } catch (err) {
      toast("Add: " + err, "error");
    }
  });

  // ─── wire up ────────────────────────────────────────────────────

  listen("status-changed", () => {
    refreshServer();
    refreshBridge();
  });

  refreshServer();
  refreshBridge();
  // Light polling for bridge status (child processes can die
  // between events; the WS reconnect loop happens inside the bridge).
  setInterval(refreshBridge, 5000);

  console.log("[tianshu-launcher] UI ready");
})();
