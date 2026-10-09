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

  let serverStarting = false;

  function renderServer() {
    const running = serverState.server_running;
    serverEls.dot.classList.toggle("on", running);
    serverEls.dot.classList.toggle("starting", serverStarting);
    serverEls.sub.textContent = serverStarting
      ? "Starting\u2026 waiting for port " + serverState.server_port
      : running
        ? "Running on port " + serverState.server_port
        : "Stopped";
    serverEls.btn.textContent = serverStarting ? "Starting\u2026" : running ? "Stop" : "Start";
    serverEls.btn.classList.toggle("primary", !running && !serverStarting);
    serverEls.btn.disabled = serverStarting;
    serverEls.openBtn.disabled = !running || serverStarting;
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
    if (serverStarting) return;
    if (!serverState.server_running) {
      serverStarting = true;
      renderServer();
      try {
        serverState = await invoke("start_server");
        if (serverState.server_running) {
          toast("Server ready on port " + serverState.server_port);
        } else {
          toast("Server failed to start", "error");
        }
      } catch (err) {
        toast("Server: " + err, "error");
        console.error(err);
      } finally {
        serverStarting = false;
        renderServer();
      }
    } else {
      serverEls.btn.disabled = true;
      try {
        serverState = await invoke("stop_server");
      } catch (err) {
        toast("Server: " + err, "error");
        console.error(err);
      } finally {
        serverEls.btn.disabled = false;
        renderServer();
      }
    }
  });

  serverEls.openBtn.addEventListener("click", async () => {
    try { await invoke("open_web_ui"); }
    catch (err) { toast("Open: " + err, "error"); }
  });

  // ─── bridge tab ─────────────────────────────────────────────────

  const listEl = document.getElementById("profile-list");
  const badgeEl = document.getElementById("bridge-badge");
  const addBtn = document.getElementById("add-profile-btn");
  const pasteBtn = document.getElementById("paste-profile-btn");
  const readClipboard = tauri?.clipboardManager?.readText;
  const openEdits = new Set();
  const editDrafts = new Map();

  let profiles = [];
  let statusMap = new Map();

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

    const engineRow = document.createElement("div");
    engineRow.className = "form-row";
    const eLabel = document.createElement("label");
    eLabel.textContent = "Browser";
    engineRow.appendChild(eLabel);
    const sel = document.createElement("select");
    const engineLabels = {
      own: "own \u2014 your local Chrome (via CDP)",
      stealth: "stealth \u2014 CloakBrowser (anti-bot, ~200MB first run)",
    };
    ["own", "stealth"].forEach((v) => {
      const opt = document.createElement("option");
      opt.value = v;
      opt.textContent = engineLabels[v];
      if (draft.engine === v) opt.selected = true;
      sel.appendChild(opt);
    });
    sel.addEventListener("change", () => { draft.engine = sel.value; });
    engineRow.appendChild(sel);
    form.appendChild(engineRow);

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

    const actions = document.createElement("div");
    actions.className = "form-row actions";

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
        if (!draft.name?.trim()) {
          toast("Name required", "error");
          return;
        }
        if (!/^wss?:\/\//i.test(draft.server || "")) {
          toast("Server must start with ws:// or wss://", "error");
          return;
        }
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

  function parseInvite(raw) {
    const trimmed = raw.trim();
    if (!trimmed) return null;
    if (/^tsbridge:\/\//i.test(trimmed)) {
      try {
        const u = new URL(trimmed);
        const p = u.searchParams;
        const bool01 = (v) => v === "1" || v === "true";
        const out = {
          server: p.get("server") || "",
          token: p.get("token") || "",
          device: p.get("device") || "",
        };
        if (p.has("browser")) out.browser = bool01(p.get("browser"));
        if (p.has("engine")) out.engine = p.get("engine");
        if (p.has("headless")) out.headless = bool01(p.get("headless"));
        if (p.has("shell")) out.shell = bool01(p.get("shell"));
        return out;
      } catch { return null; }
    }
    if (trimmed.startsWith("{")) {
      try { return JSON.parse(trimmed); } catch { return null; }
    }
    if (/^wss?:\/\//i.test(trimmed)) {
      const parts = trimmed.split(/\s+/);
      return { server: parts[0], token: parts[1] || "" };
    }
    if (/tsbridge\b|--server\b/.test(trimmed)) {
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

  // ─── TTS Server (compact) ──────────────────────────────────────

  const ttsDot = document.getElementById("tts-dot");
  const ttsSub = document.getElementById("tts-sub");
  const ttsActions = document.getElementById("tts-actions");
  const ttsProgressEl = document.getElementById("tts-progress");

  function renderTtsCompact(s) {
    ttsActions.innerHTML = "";

    if (!s.installed) {
      ttsDot.className = "dot";
      ttsSub.textContent = "Not installed";
      const btn = document.createElement("button");
      btn.className = "primary";
      btn.textContent = "Install";
      btn.addEventListener("click", doInstallTts);
      ttsActions.appendChild(btn);
    } else if (s.running && s.ready) {
      ttsDot.className = "dot on";
      ttsSub.textContent = "Ready \u00b7 Port " + s.port;
      const btn = document.createElement("button");
      btn.textContent = "Stop";
      btn.addEventListener("click", doStopTts);
      ttsActions.appendChild(btn);
    } else if (s.running) {
      ttsDot.className = "dot starting";
      ttsSub.textContent = "Starting on :" + s.port + "\u2026";
      const btn = document.createElement("button");
      btn.textContent = "Stop";
      btn.addEventListener("click", doStopTts);
      ttsActions.appendChild(btn);
    } else {
      ttsDot.className = "dot";
      ttsSub.textContent = "Stopped";
      const startBtn = document.createElement("button");
      startBtn.className = "primary";
      startBtn.textContent = "Start";
      startBtn.addEventListener("click", doStartTts);
      ttsActions.appendChild(startBtn);
      const reinstallBtn = document.createElement("button");
      reinstallBtn.textContent = "Reinstall";
      reinstallBtn.addEventListener("click", doInstallTts);
      ttsActions.appendChild(reinstallBtn);
    }
  }

  async function doInstallTts() {
    ttsActions.innerHTML = "";
    ttsDot.className = "dot starting";
    ttsSub.textContent = "Installing\u2026";
    ttsProgressEl.style.display = "";
    ttsProgressEl.textContent = "Downloading ~1.2 GB model, this may take a few minutes\u2026";
    try {
      const result = await invoke("install_tts");
      ttsProgressEl.textContent = "\u2713 " + (result || "Installed successfully");
      toast("Qwen3-TTS installed");
      refreshTts();
    } catch (err) {
      ttsProgressEl.innerHTML = "";
      ttsProgressEl.textContent = "\u2717 " + err;
      const logBtn = document.createElement("button");
      logBtn.textContent = "View Install Log";
      logBtn.style.cssText = "margin-top:6px;font-size:11px;padding:3px 8px;display:block";
      logBtn.addEventListener("click", async () => {
        try {
          const log = await invoke("tts_install_log");
          const pre = document.createElement("pre");
          pre.style.cssText = "max-height:200px;overflow:auto;font-size:10px;padding:6px;" +
            "background:var(--bg);border:1px solid var(--border);border-radius:4px;margin-top:4px;" +
            "white-space:pre-wrap;word-break:break-all;";
          pre.textContent = log;
          logBtn.replaceWith(pre);
        } catch (e) {
          toast("Failed to read log: " + e, "error");
        }
      });
      ttsProgressEl.appendChild(logBtn);
      toast("Install failed: " + err, "error");
      refreshTts();
    }
  }

  async function doStartTts() {
    ttsDot.className = "dot starting";
    ttsSub.textContent = "Starting\u2026";
    ttsActions.innerHTML = "";
    try {
      await invoke("start_tts");
      toast("TTS server started");
    } catch (err) {
      toast("Failed to start TTS: " + err, "error");
    }
    refreshTts();
  }

  async function doStopTts() {
    try {
      await invoke("stop_tts");
      toast("TTS server stopped");
    } catch (err) {
      toast("Failed to stop TTS: " + err, "error");
    }
    refreshTts();
  }

  async function refreshTts() {
    try {
      const s = await invoke("tts_status");
      renderTtsCompact(s);
    } catch (err) {
      ttsDot.className = "dot";
      ttsSub.textContent = "Error";
      ttsActions.innerHTML = "";
    }
  }

  // ─── updates (banner + expanded panel) ─────────────────────────

  const updateBanner = document.getElementById("update-banner");
  const updateBannerText = document.getElementById("update-banner-text");
  const updateBannerBtn = document.getElementById("update-banner-btn");
  const updatesPanel = document.getElementById("updates-panel");
  const updatesTitle = document.getElementById("updates-title");
  const updatesBody = document.getElementById("updates-body");
  const checkUpdatesBtn = document.getElementById("check-updates-btn");
  const closeUpdatesBtn = document.getElementById("close-updates-btn");

  let lastReport = null;

  const COMPONENT_INSTALL_MAP = {
    "Tianshu Server": { sub: "server", package: "@tianshu-ai/tianshu" },
    "Local Bridge": { sub: "bridge", package: "@tianshu-ai/local-bridge" },
  };

  function showUpdateBanner(report) {
    if (!report || !report.any_update) {
      updateBanner.style.display = "none";
      return;
    }
    const updates = report.components.filter((c) => c.update_available);
    if (updates.length === 1) {
      updateBannerText.textContent =
        "\u2B06 " + updates[0].name + " " + updates[0].latest + " available";
    } else {
      updateBannerText.textContent = "\u2B06 " + updates.length + " updates available";
    }
    updateBanner.style.display = "";
  }

  updateBannerBtn.addEventListener("click", () => {
    if (lastReport) {
      updateBanner.style.display = "none";
      updatesPanel.style.display = "";
      renderUpdates(lastReport);
    }
  });

  closeUpdatesBtn.addEventListener("click", () => {
    updatesPanel.style.display = "none";
    if (lastReport && lastReport.any_update) {
      showUpdateBanner(lastReport);
    }
  });

  function renderUpdates(report) {
    updatesBody.innerHTML = "";
    updatesTitle.className = "";
    if (!report) {
      updatesTitle.textContent = "Updates";
      return;
    }
    report.components.forEach((c) => {
      const row = document.createElement("div");
      row.className = "version-row";
      const name = document.createElement("span");
      name.className = "name";
      name.textContent = c.name;
      row.appendChild(name);
      const vers = document.createElement("span");
      vers.className = "vers";
      if (c.update_available) {
        vers.innerHTML =
          c.current + " \u2192 <span class=\"new\">" + c.latest + "</span>";
      } else if (c.latest === "unknown") {
        vers.innerHTML = c.current + " <span class=\"tag\">\u2014</span>";
      } else {
        const tagLabel = report.channel === "next" ? "next" : "latest";
        vers.innerHTML = c.current + " <span class=\"tag\">" + tagLabel + "</span>";
      }
      row.appendChild(vers);
      updatesBody.appendChild(row);
    });

    if (report.any_update) {
      updatesTitle.textContent = "Updates available";
      updatesTitle.className = "warn";
      const actions = document.createElement("div");
      actions.className = "updates-actions";
      const updateAllBtn = document.createElement("button");
      updateAllBtn.className = "primary";
      updateAllBtn.textContent = "Update All";
      updateAllBtn.addEventListener("click", () => applyUpdates(report));
      actions.appendChild(updateAllBtn);
      updatesBody.appendChild(actions);
    } else {
      updatesTitle.textContent = "All up to date";
      updatesTitle.className = "success";
    }
  }

  async function applyUpdates(report) {
    const updates = report.components.filter((c) => c.update_available);
    if (updates.length === 0) return;

    const launcherUpdate = updates.find((c) => c.name === "Launcher");
    const payloadUpdates = updates.filter((c) => c.name !== "Launcher");

    checkUpdatesBtn.disabled = true;
    updatesTitle.textContent = "Updating\u2026";

    const progressEl = document.createElement("div");
    progressEl.className = "update-progress";
    updatesBody.innerHTML = "";
    updatesBody.appendChild(progressEl);

    function addLine(text, status) {
      const row = document.createElement("div");
      row.className = "update-row " + status;
      if (status === "pending") {
        const spinner = document.createElement("span");
        spinner.className = "spinner";
        row.appendChild(spinner);
        const label = document.createElement("span");
        label.textContent = text;
        row.appendChild(label);
        const elapsed = document.createElement("span");
        elapsed.className = "elapsed";
        elapsed.textContent = "0s";
        row.appendChild(elapsed);
        const t0 = Date.now();
        row._timer = setInterval(() => {
          elapsed.textContent = Math.round((Date.now() - t0) / 1000) + "s";
        }, 1000);
      } else {
        row.textContent = text;
      }
      progressEl.appendChild(row);
      return row;
    }

    function updateLine(row, text, status) {
      if (row._timer) { clearInterval(row._timer); row._timer = null; }
      row.innerHTML = "";
      row.textContent = text;
      row.className = "update-row " + status;
    }

    let allOk = true;
    try {
      for (const c of payloadUpdates) {
        const spec = COMPONENT_INSTALL_MAP[c.name];
        if (!spec) continue;
        const row = addLine(
          c.name + ": installing " + (c.latest || "latest") + "\u2026",
          "pending"
        );
        try {
          await invoke("update_payload", { sub: spec.sub, package: spec.package });
          updateLine(
            row,
            c.name + ": \u2713 updated to " + (c.latest || "latest"),
            "ok"
          );
        } catch (err) {
          updateLine(row, c.name + ": \u2717 " + err, "fail");
          allOk = false;
        }
      }
      if (launcherUpdate) {
        const launcherRow = addLine(
          "Launcher: downloading " + (launcherUpdate.latest || "latest") + "\u2026",
          "pending"
        );
        try {
          await invoke("update_launcher");
          updateLine(
            launcherRow,
            "Launcher: \u2713 updated to " + (launcherUpdate.latest || "latest") + " \u2014 restarting\u2026",
            "ok"
          );
        } catch (err) {
          updateLine(launcherRow, "Launcher: \u2717 " + err, "fail");
          allOk = false;
        }
      }

      if (allOk && (payloadUpdates.length > 0 || launcherUpdate)) {
        try { await invoke("set_update_badge", { count: 0 }); } catch (_) {}
        addLine("Restarting to apply updates\u2026", "pending");
        await new Promise((r) => setTimeout(r, 600));
        try {
          await invoke("restart_launcher");
        } catch (_) {
          updatesTitle.textContent = "Updated";
          updatesTitle.className = "success";
          checkUpdatesBtn.disabled = false;
        }
      } else if (!allOk) {
        updatesTitle.textContent = "Some updates failed";
        updatesTitle.className = "warn";
        checkUpdatesBtn.disabled = false;
      } else {
        updatesTitle.textContent = "All up to date";
        updatesTitle.className = "success";
        checkUpdatesBtn.disabled = false;
      }
    } catch (err) {
      toast("Update failed: " + err, "error");
      console.error(err);
      checkUpdatesBtn.disabled = false;
      updatesTitle.textContent = "Update failed";
    }
  }

  // Settings tab: manual check button
  checkUpdatesBtn.addEventListener("click", async () => {
    checkUpdatesBtn.disabled = true;
    checkUpdatesBtn.textContent = "Checking\u2026";
    try {
      const report = await invoke("check_updates");
      lastReport = report;
      if (report.any_update) {
        const count = report.components.filter((c) => c.update_available).length;
        try { await invoke("set_update_badge", { count }); } catch (_) {}
        // Switch to Server tab and show expanded panel
        document.querySelectorAll(".tab").forEach((t) =>
          t.classList.toggle("active", t.dataset.tab === "server"),
        );
        document.querySelectorAll(".tab-panel").forEach((p) =>
          p.classList.toggle("active", p.id === "tab-server"),
        );
        updateBanner.style.display = "none";
        updatesPanel.style.display = "";
        renderUpdates(report);
      } else {
        try { await invoke("set_update_badge", { count: 0 }); } catch (_) {}
        toast("All up to date");
      }
    } catch (err) {
      toast("Check failed: " + err, "error");
      console.error(err);
    } finally {
      checkUpdatesBtn.disabled = false;
      checkUpdatesBtn.textContent = "Check for Updates";
    }
  });

  // Background update check: 30s after launch, then every 4 hours
  async function backgroundUpdateCheck() {
    try {
      const report = await invoke("check_updates");
      lastReport = report;
      if (report.any_update) {
        const count = report.components.filter((c) => c.update_available).length;
        try { await invoke("set_update_badge", { count }); } catch (_) {}
        showUpdateBanner(report);
      } else {
        try { await invoke("set_update_badge", { count: 0 }); } catch (_) {}
        updateBanner.style.display = "none";
      }
    } catch (err) {
      console.error("background update check:", err);
    }
  }

  setTimeout(backgroundUpdateCheck, 30000);
  setInterval(backgroundUpdateCheck, 4 * 60 * 60 * 1000);

  // ─── settings ───────────────────────────────────────────────────

  const updateChannelSelect = document.getElementById("update-channel-select");
  const registryInput = document.getElementById("npm-registry-input");
  const pipIndexInput = document.getElementById("pip-index-input");
  const pythonMirrorInput = document.getElementById("python-mirror-input");
  const saveSettingsBtn = document.getElementById("save-settings-btn");

  invoke("get_launcher_settings").then((s) => {
    if (s && s.update_channel) updateChannelSelect.value = s.update_channel;
    if (s && s.npm_registry) registryInput.value = s.npm_registry;
    if (s && s.pip_index) pipIndexInput.value = s.pip_index;
    if (s && s.python_mirror) pythonMirrorInput.value = s.python_mirror;
  }).catch(() => {});

  saveSettingsBtn.addEventListener("click", async () => {
    const channel = updateChannelSelect.value || null;
    const npm = registryInput.value.trim() || null;
    const pip = pipIndexInput.value.trim() || null;
    const pyMirror = pythonMirrorInput.value.trim() || null;
    try {
      await invoke("set_launcher_settings", {
        settings: { update_channel: channel, npm_registry: npm, pip_index: pip, python_mirror: pyMirror }
      });
      toast("Settings saved");
    } catch (err) {
      toast("Failed to save: " + err, "error");
    }
  });

  // ─── wire up ────────────────────────────────────────────────────

  listen("status-changed", () => {
    refreshServer();
    refreshBridge();
  });

  refreshServer();
  refreshBridge();
  refreshTts();
  setInterval(refreshBridge, 5000);
  setInterval(refreshTts, 10000);

  console.log("[tianshu-launcher] UI ready");
})();
