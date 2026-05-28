// Toki admin panel — vanilla JS, no build step, no dependencies.
//
// Wire contract (must stay in sync with crates/server/src/admin/dto.rs):
//   Snapshot         { rooms, lobby, generation, serverUptimeSecs }
//   RoomDto          { frequency, holder, members: MemberDto[] }
//   MemberDto        { id, displayName, lastSeenSecs }
//   ServerInfo       { version, adminBind, startedAtUnix }
//
// Architecture: a single `state` object holds the latest snapshot +
// server-info + section selection. Every render function takes that
// state and returns HTML strings; the section router replaces
// `#content` innerHTML on each SSE tick (cheap, ~kb of HTML).
// Click handling uses event delegation via data-action attributes so
// rerenders don't leak listeners.

(() => {
  "use strict";

  // ════════════════════════════════════════════════════════════════
  // Constants — the band Toki uses. Mirrors `validation.rs` server-side.
  // ════════════════════════════════════════════════════════════════

  const FREQ_MIN = 446.00;
  const FREQ_MAX = 448.00;
  const FREQ_STEP = 0.05;

  /** All 41 frequencies in the band, as canonical "446.05" strings.
   *  Channels.list renders all of these so the operator sees the
   *  whole band, not just the rooms that happen to have members. */
  const ALL_FREQS = (() => {
    const out = [];
    for (let i = 0; ; i++) {
      const f = +(FREQ_MIN + i * FREQ_STEP).toFixed(2);
      if (f > FREQ_MAX + 1e-6) break;
      out.push(f.toFixed(2));
    }
    return out;
  })();

  /** Channel number (1-based) for a given canonical frequency string,
   *  or null if it's outside the band. */
  function chanNumOf(freq) {
    const idx = ALL_FREQS.indexOf(freq);
    return idx < 0 ? null : idx + 1;
  }

  /** Format a UNIX timestamp as YYYY-MM-DD (local). */
  function fmtDate(unix) {
    if (!unix) return "—";
    const d = new Date(unix * 1000);
    const yyyy = d.getFullYear();
    const mm = String(d.getMonth() + 1).padStart(2, "0");
    const dd = String(d.getDate()).padStart(2, "0");
    return `${yyyy}-${mm}-${dd}`;
  }

  /** Format seconds as "Nd HH:MM:SS" — matches the design's UPTIME chip. */
  function fmtUptime(s) {
    if (s == null || s < 0) return "—";
    const d = Math.floor(s / 86400); s -= d * 86400;
    const h = Math.floor(s / 3600);  s -= h * 3600;
    const m = Math.floor(s / 60);    s -= m * 60;
    return `${d}d ${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  }

  /** Format seconds-since as a coarse "Ns ago" / "Nm ago" string. */
  function fmtAgo(secs) {
    if (secs == null) return "—";
    if (secs < 60) return `${secs}s ago`;
    const m = Math.floor(secs / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.floor(m / 60);
    return `${h}h ago`;
  }

  /** Tiny escape for any string we shove into innerHTML. */
  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#039;");
  }

  // ════════════════════════════════════════════════════════════════
  // State + DOM hooks
  // ════════════════════════════════════════════════════════════════

  const SECTIONS = [
    { id: "overview", label: "Overview",  num: "01" },
    { id: "channels", label: "Channels",  num: "02" },
    { id: "server",   label: "Server",    num: "03" },
    { id: "audit",    label: "Audit Log", num: "04" },
  ];

  const state = {
    section: "overview",
    snap: null,
    serverInfo: null,
    selectedFreq: null,    // canonical "446.05" string, in Channels view
    actionMenu: null,      // { peerId, freq, anchor: DOMRect, submenu: bool }
    filter: "",            // channels-list search box content
    /**
     * When true, the channel list hides frequencies with zero members
     * (the band still has 41 possible channels but most of them are
     * empty most of the time, so on by default makes the list usable
     * at-a-glance). Toggled via the "Active only" checkbox in the
     * list header.
     */
    activeOnly: true,
    /**
     * Live server config (server_name, max_peers, idle_kick_secs).
     * Loaded lazily when the Server section opens; nulled out so a
     * stale value never lingers if the API call hadn't yet returned.
     * Mutated on Save via PUT /api/server-config.
     */
    serverConfig: null,
    /** True while a server-config save POST is in flight. */
    serverConfigSaving: false,
    /** True while a change-password POST is in flight. */
    pwSaving: false,
    /** True while a server-password PUT is in flight. */
    serverPwSaving: false,
    /** Reveal toggle for the server password input. The admin needs
     *  to know the actual value to share it with their users, so we
     *  let them flip the input between password/text type. */
    serverPwReveal: false,
  };

  const $ = (sel) => document.querySelector(sel);
  const main = $("#main");
  const content = $("#content");
  const toastEl = $("#toast");

  let evtSource = null;

  // ════════════════════════════════════════════════════════════════
  // HTTP helpers
  // ════════════════════════════════════════════════════════════════

  async function api(method, path, body) {
    const res = await fetch(path, {
      method,
      credentials: "same-origin",
      headers: body ? { "content-type": "application/json" } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!res.ok) {
      let msg = `${res.status} ${res.statusText}`;
      try {
        const j = await res.json();
        if (j && j.error) msg = j.error;
      } catch {}
      throw new Error(msg);
    }
    if (res.status === 204) return null;
    return res.json();
  }

  // ════════════════════════════════════════════════════════════════
  // Bootstrap + view switching
  // ════════════════════════════════════════════════════════════════

  function showView(view) {
    main.dataset.view = view;
  }

  async function start() {
    showView("loading");
    try {
      const [snap, info] = await Promise.all([
        api("GET", "/api/state"),
        api("GET", "/api/server-info"),
      ]);
      state.snap = snap;
      state.serverInfo = info;
      enterDashboard();
    } catch (e) {
      showView("login");
      if (!/^401/.test(e.message)) {
        const el = $("#login-error");
        el.textContent = e.message;
        el.hidden = false;
      }
    }
  }

  $("#login-form").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    const errEl = $("#login-error");
    errEl.hidden = true;
    try {
      await api("POST", "/api/login", {
        username: fd.get("username"),
        password: fd.get("password"),
      });
      ev.target.reset();
      start();
    } catch (e) {
      errEl.textContent = e.message;
      errEl.hidden = false;
    }
  });

  async function logout() {
    try { await api("POST", "/api/logout"); } catch {}
    if (evtSource) { evtSource.close(); evtSource = null; }
    state.snap = null;
    state.serverInfo = null;
    showView("login");
  }

  function enterDashboard() {
    showView("dashboard");
    $("#topbar").style.display = "";
    $("#sidebar").style.display = "";
    content.style.display = "";
    renderShell();
    render();
    openEventStream();
  }

  function openEventStream() {
    if (evtSource) evtSource.close();
    evtSource = new EventSource("/api/events");
    evtSource.addEventListener("state", (e) => {
      try {
        state.snap = JSON.parse(e.data);
        // The topbar (host / uptime / peers / TX) lives outside
        // `#content` and always reflects the latest tick.
        renderHeaderLive();
        // Section content only repaints when it's displaying live
        // data. Settings sections (server, audit) stay put so an
        // in-progress form edit isn't blown away every second.
        if (LIVE_SECTIONS.has(state.section)) {
          renderContent();
        }
        setConn("live");
      } catch {}
    });
    evtSource.addEventListener("error", () => setConn("bad"));
  }

  function setConn(which) {
    const pill = $("#conn-pill");
    const label = $("#conn-label");
    if (which === "live") {
      pill.classList.remove("bad");
      label.textContent = "CONNECTED";
      pill.title = "live feed connected";
    } else {
      pill.classList.add("bad");
      label.textContent = "RECONNECTING";
      pill.title = "live feed reconnecting…";
    }
  }

  function toast(msg, tone = "phos") {
    toastEl.className = `toast ${tone === "amber" ? "amber" : tone === "red" ? "red" : ""}`;
    toastEl.textContent = msg;
    toastEl.classList.remove("hidden");
    clearTimeout(toast._t);
    toast._t = setTimeout(() => toastEl.classList.add("hidden"), 2200);
  }

  // ════════════════════════════════════════════════════════════════
  // Header / sidebar / shell (parts that don't change per SSE tick)
  // ════════════════════════════════════════════════════════════════

  function renderShell() {
    if (state.serverInfo) {
      $("#brand-version").textContent = `v${state.serverInfo.version}`;
      $("#stat-host").textContent = state.serverInfo.adminBind;
      $("#build-line").textContent = `v${state.serverInfo.version}`;
      $("#started-line").textContent = `since ${fmtDate(state.serverInfo.startedAtUnix)}`;
    }
    const navList = $("#nav-list");
    navList.innerHTML = SECTIONS.map((s) => `
      <li>
        <button class="nav-btn ${s.id === state.section ? "active" : ""}" data-section="${s.id}">
          <span class="nav-num">${s.num}</span>
          <span class="nav-label">${esc(s.label)}</span>
          <span class="nav-count" id="nav-count-${s.id}"></span>
        </button>
      </li>`).join("");
  }

  function renderHeaderLive() {
    if (!state.snap) return;
    const peers = (state.snap.rooms || []).reduce((n, r) => n + r.members.length, 0)
                + (state.snap.lobby || []).length;
    const tx = (state.snap.rooms || []).filter((r) => r.holder).length;
    $("#stat-uptime").textContent = fmtUptime(state.snap.serverUptimeSecs);
    $("#stat-peers").textContent = peers + (peers === 1 ? " online" : " online");
    const txEl = $("#stat-tx");
    txEl.textContent = tx + (tx > 0 ? " active" : " idle");
    txEl.className = "stat-value " + (tx > 0 ? "amber" : "dim");
    // Sidebar counts
    const channelsLive = (state.snap.rooms || []).length;
    setNavCount("channels", channelsLive);
    setNavCount("overview", "");
    setNavCount("server", "");
    setNavCount("audit", "");
  }
  function setNavCount(id, val) {
    const el = document.getElementById(`nav-count-${id}`);
    if (el) el.textContent = val === "" ? "" : String(val);
  }

  // ════════════════════════════════════════════════════════════════
  // Main render — called on every SSE snapshot + on section change
  // ════════════════════════════════════════════════════════════════

  /** Sections whose content reflects live snapshot data (peer counts,
   *  PTT state, member rosters, etc.). The SSE handler only re-renders
   *  these on every tick — settings pages (server, audit) stay put so
   *  in-progress edits aren't wiped, and password inputs in particular
   *  don't reset back to empty under the user's typing fingers.
   *
   *  Explicit user actions (clicking a nav item, hitting Save) still
   *  go through `render()` which re-renders unconditionally. */
  const LIVE_SECTIONS = new Set(["overview", "channels"]);

  function render() {
    renderHeaderLive();
    renderContent();
  }

  /** Section-content render only. Splits out from `render()` so SSE
   *  ticks can update the topbar without blowing away in-progress
   *  form input in the Server / Audit sections. */
  function renderContent() {
    let html;
    switch (state.section) {
      case "overview": html = renderOverview(); break;
      case "channels": html = renderChannels(); break;
      case "server":   html = renderServer();   break;
      case "audit":    html = renderAudit();    break;
      default:         html = renderOverview();
    }
    // Preserve focus + selection across the innerHTML swap. The
    // Channels filter input and the Server config inputs would
    // otherwise lose focus on every render, making them unusable.
    // We snapshot the active element's id + selection range, swap,
    // then restore.
    const focused = document.activeElement;
    const focusedId = focused && focused.id ? focused.id : null;
    let selStart = null, selEnd = null;
    if (focused && typeof focused.selectionStart === "number") {
      selStart = focused.selectionStart;
      selEnd = focused.selectionEnd;
    }
    content.innerHTML = html;
    if (focusedId) {
      const refocused = document.getElementById(focusedId);
      if (refocused) {
        refocused.focus();
        if (selStart !== null) {
          try { refocused.setSelectionRange(selStart, selEnd); } catch {}
        }
      }
    }
  }

  // ── Overview ────────────────────────────────────────────────────

  function renderOverview() {
    const snap = state.snap || { rooms: [], lobby: [], serverUptimeSecs: 0 };
    const info = state.serverInfo || { version: "—", adminBind: "—", startedAtUnix: 0 };
    const peers = snap.rooms.reduce((n, r) => n + r.members.length, 0) + snap.lobby.length;
    const tx = snap.rooms.filter((r) => r.holder).length;
    const channelsLive = snap.rooms.length;

    const busy = [...snap.rooms]
      .sort((a, b) => b.members.length - a.members.length)
      .slice(0, 8);
    const maxN = busy[0]?.members.length || 1;

    return `
      ${sectionHead("01 · OVERVIEW", "Server overview", `${info.adminBind} · started ${fmtDate(info.startedAtUnix)}`)}
      <div class="scroll">
        <div class="kpi-grid">
          ${kpi("UPTIME", fmtUptime(snap.serverUptimeSecs), `since ${fmtDate(info.startedAtUnix)}`, "phos")}
          ${kpi("PEERS ONLINE", peers, peers === 1 ? "1 client" : `${peers} clients`, "default")}
          ${kpi("TRANSMITTING", tx, tx > 0 ? "active right now" : "silent", tx > 0 ? "amber" : "dim")}
          ${kpi("ACTIVE CHANS", channelsLive, `of ${ALL_FREQS.length} possible`, "default")}
        </div>

        <div class="split">
          <div class="card">
            ${cardHead("Busiest channels", "TOP 8", `<button class="btn ghost sm" data-section="channels">all channels →</button>`)}
            ${busy.length === 0
              ? `<p class="placeholder">No channels currently have members.</p>`
              : `<ul class="busy-list">${busy.map((r) => `
                  <li class="busy-row">
                    <span class="ch-num">${String(chanNumOf(r.frequency) || "").padStart(2, "0")}</span>
                    <span class="freq">${esc(r.frequency)}<span class="unit">MHz</span></span>
                    <div class="busy-track"><i style="width: ${(r.members.length / maxN) * 100}%;"></i></div>
                    <span class="count">${r.members.length}</span>
                  </li>`).join("")}</ul>`}
          </div>
          <div class="card">
            ${cardHead("Lobby", "PRE-JOIN")}
            ${snap.lobby.length === 0
              ? `<p class="placeholder">No clients sitting between Register and Join.</p>`
              : `<ul class="busy-list">${snap.lobby.map((m) => `
                  <li class="busy-row">
                    <span class="freq" style="width: auto;">${esc(m.displayName)}</span>
                    <div class="busy-track"></div>
                    <span class="count" style="color: var(--ink-mute);">${m.lastSeenSecs}s</span>
                  </li>`).join("")}</ul>`}
          </div>
        </div>

        <div class="split">
          <div class="card">
            ${cardHead("Recent activity", "AUDIT")}
            <p class="placeholder">Audit-log capture is not yet implemented server-side. Once it is, recent admin actions will surface here.</p>
          </div>
          <div class="card">
            ${cardHead("Server health", "INFO")}
            <p class="placeholder">Per-process CPU / memory / I/O telemetry isn't tracked yet. Use <code>RUST_LOG=info</code> on the server for ad-hoc diagnostics.</p>
          </div>
        </div>
      </div>
    `;
  }

  function kpi(label, value, sub, tone) {
    return `
      <div class="kpi">
        <span class="label">${esc(label)}</span>
        <span class="value ${tone}">${esc(value)}</span>
        <span class="sub">${esc(sub)}</span>
      </div>`;
  }

  // ── Channels ────────────────────────────────────────────────────

  function renderChannels() {
    const snap = state.snap || { rooms: [], lobby: [] };
    // Build a map of frequency -> RoomDto for O(1) lookup.
    const byFreq = new Map(snap.rooms.map((r) => [r.frequency, r]));

    // Pick a default selection: first room with members, else first
    // freq in the band. Stays sticky once the operator picks one.
    if (!state.selectedFreq) {
      state.selectedFreq = (snap.rooms[0]?.frequency) || ALL_FREQS[0];
    }
    const selected = state.selectedFreq;
    const room = byFreq.get(selected); // may be undefined for empty channels

    // Two stacked filters: the "active only" checkbox hides empty
    // channels, and the search box matches against the freq string
    // or 2-digit channel number. The intersection is what we render.
    const q = state.filter.trim().toLowerCase();
    const visible = ALL_FREQS.filter((f) => {
      if (state.activeOnly && !byFreq.has(f)) return false;
      if (!q) return true;
      return f.includes(q) || String(chanNumOf(f)).padStart(2, "0").includes(q);
    });

    const listRows = visible.map((f) => {
      const r = byFreq.get(f);
      const live = r ? r.members.length : 0;
      const tx = r && r.holder ? true : false;
      const active = f === selected;
      const num = String(chanNumOf(f)).padStart(2, "0");
      return `
        <li>
          <button class="chan-row ${active ? "active" : ""}" data-select-freq="${esc(f)}">
            <span class="num">${num}</span>
            <div class="body">
              <div class="freq-line"><span class="freq">${esc(f)}</span><span class="unit">MHz</span></div>
              <span class="desc">Channel ${num} · auto-formed</span>
            </div>
            <div class="right">
              ${tx ? `<span class="tx-dot" title="someone is transmitting"></span>` : ""}
              <span class="live ${live > 0 ? "live-on" : ""}">${live}</span>
            </div>
          </button>
        </li>`;
    }).join("");

    const memberCount = room?.members.length || 0;
    const activeCount = room?.holder ? 1 : 0;
    const members = room?.members.slice().sort((a, b) => {
      const ta = a.id === room.holder ? -1 : 0;
      const tb = b.id === room.holder ? -1 : 0;
      if (ta !== tb) return ta - tb;
      return a.displayName.localeCompare(b.displayName);
    }) || [];

    const memberRows = members.map((m) => {
      const isHolder = room?.holder === m.id;
      const stateClass = isHolder ? "tx" : "rx";
      const stateLabel = isHolder ? "● TX" : "◐ RX";
      return `
        <li class="member-row">
          <span class="state-dot ${stateClass}"></span>
          <span class="callsign">${esc(m.displayName)}</span>
          <span class="state-label ${stateClass}">${stateLabel}</span>
          <span class="seen">${m.lastSeenSecs}s ago</span>
          <span class="id">${esc(m.id)}</span>
          <button class="row-menu-btn" data-menu="${esc(m.id)}" title="Member actions">…</button>
        </li>`;
    }).join("");

    return `
      ${sectionHead("02 · CHANNELS", "Channels", `${ALL_FREQS.length} possible · ${snap.rooms.length} active`)}
      <div class="channels-split">
        <aside class="chan-list-col">
          <div class="list-search">
            <span class="slash">/</span>
            <input id="chan-filter" placeholder="filter frequencies…" value="${esc(state.filter)}" />
            <span class="hint">${visible.length}</span>
          </div>
          <div class="list-filter">
            <label>
              <input type="checkbox" id="chan-active-only" ${state.activeOnly ? "checked" : ""} />
              <span>Active only</span>
            </label>
            <span class="filter-count">${snap.rooms.length} of ${ALL_FREQS.length}</span>
          </div>
          <div class="list-head">
            <span style="width: 24px;">#</span>
            <span style="flex: 1;">FREQUENCY</span>
            <span style="width: 36px; text-align: right;">LIVE</span>
          </div>
          <ul class="chan-list">${
            visible.length === 0
              ? `<li class="chan-list-empty">${
                  state.activeOnly && !q
                    ? "No active channels yet."
                    : "No channels match your filter."
                }</li>`
              : listRows
          }</ul>
        </aside>

        <main class="chan-detail-col">
          <header class="chan-detail-head">
            <div class="left">
              <div class="detail-num-block">
                <span class="label">CH</span>
                <span class="num">${String(chanNumOf(selected) || "—").padStart(2, "0")}</span>
              </div>
              <div class="chan-detail-title">
                <h3>${esc(selected)}<span class="unit">MHz</span></h3>
                <span class="desc">Channel ${String(chanNumOf(selected) || "").padStart(2, "0")} · auto-formed on first join</span>
              </div>
            </div>
            <div class="chan-detail-stats">
              <div class="big-stat"><span class="label">MEMBERS</span><span class="value">${memberCount}</span></div>
              <div class="big-stat"><span class="label">LIVE</span><span class="value phos">${activeCount}</span></div>
            </div>
          </header>

          <section class="members">
            <div class="members-head">
              <div>
                <span class="tag-text">· MEMBERS ·</span>
                <span class="count">${memberCount} on channel</span>
              </div>
            </div>
            ${memberCount === 0
              ? `<div class="members-empty">No members on this frequency.</div>`
              : `<div class="table-head" style="padding-top: 0;">
                  <span style="width: 8px;"></span>
                  <span style="flex: 2;">CALLSIGN</span>
                  <span style="flex: 1;">STATE</span>
                  <span style="flex: 1;">LAST SEEN</span>
                  <span style="flex: 2;">CLIENT ID</span>
                  <span style="width: 36px;"></span>
                </div>
                <ul class="table-body">${memberRows}</ul>`}
          </section>
        </main>
      </div>
    `;
  }

  // ── Server (live runtime config — editable) ─────────────────────
  //
  // Two cards:
  //   * "Bootstrap" — read-only values that come from config.toml /
  //     env / TLS files. Edited by SSH-ing into the box and restarting.
  //   * "Runtime" — editable. Fields back the `ServerConfig` table in
  //     admin.db; Save POSTs to PUT /api/server-config which writes
  //     the row AND updates the shared in-memory handle, so signaling
  //     + reaper see the new values on their next read.
  //
  // The form is rendered from `state.serverConfig` so it survives a
  // re-render mid-edit (after an SSE tick repaints the topbar etc).
  // Pending edits live in the DOM inputs until Save fires.

  function renderServer() {
    const info = state.serverInfo || {};
    const cfg = state.serverConfig;
    return `
      ${sectionHead("03 · SERVER", "Server", "Bootstrap values live in config.toml; runtime tunables persist in admin.db")}
      <div class="scroll">
        <div class="options-grid">
          <div class="card">
            ${cardHead("Bootstrap", "A", `<span class="placeholder" style="padding:0;">Read-only · restart to change</span>`)}
            <ul class="cfg-list">
              ${cfgRow("VERSION", info.version ? `toki-server v${info.version}` : "—")}
              ${cfgRow("ADMIN BIND", info.adminBind || "—")}
              ${cfgRow("STARTED AT", fmtDate(info.startedAtUnix))}
              ${cfgRow("UPTIME", fmtUptime(state.snap?.serverUptimeSecs || 0))}
            </ul>
          </div>

          <div class="card">
            ${cardHead("Runtime", "B", `<button class="btn phos sm" data-server-save ${cfg && !state.serverConfigSaving ? "" : "disabled"}>${state.serverConfigSaving ? "saving…" : "Save changes"}</button>`)}
            ${cfg
              ? `<ul class="cfg-list" id="server-config-form">
                   <li class="cfg-row">
                     <span class="cfg-label">SERVER NAME</span>
                     <input class="input cfg-input" id="cfg-server-name" type="text" maxlength="64" value="${esc(cfg.serverName)}" placeholder="(optional)" />
                   </li>
                   <li class="cfg-row">
                     <span class="cfg-label">MAX PEERS</span>
                     <input class="input cfg-input num-input" id="cfg-max-peers" type="number" min="1" max="100000" value="${cfg.maxPeers}" />
                   </li>
                   <li class="cfg-row">
                     <span class="cfg-label">IDLE KICK (sec)</span>
                     <input class="input cfg-input num-input" id="cfg-idle-kick" type="number" min="5" max="86400" value="${cfg.idleKickSecs}" />
                   </li>
                 </ul>
                 <p class="placeholder" style="margin-top: 14px;">
                   <code>max_peers</code> caps the registry size — Register RPCs above it return RESOURCE_EXHAUSTED.
                   <code>idle_kick_secs</code> is how long the reaper waits before evicting a client that's stopped
                   sending keepalives.
                 </p>`
              : `<p class="placeholder">Loading…</p>`}
          </div>

          <div class="card">
            ${cardHead("Admin password", "C", `<button class="btn phos sm" data-pw-save ${state.pwSaving ? "disabled" : ""}>${state.pwSaving ? "saving…" : "Change admin password"}</button>`)}
            <p class="placeholder" style="margin-top: 0; margin-bottom: 10px;">
              Controls who can sign in to <em>this admin panel</em>.
              Not the same as the gRPC client password below.
            </p>
            <ul class="cfg-list" id="pw-form">
              <li class="cfg-row">
                <span class="cfg-label">CURRENT</span>
                <input class="input cfg-input" id="pw-current" type="password" autocomplete="current-password" />
              </li>
              <li class="cfg-row">
                <span class="cfg-label">NEW</span>
                <input class="input cfg-input" id="pw-new" type="password" autocomplete="new-password" minlength="8" maxlength="128" />
              </li>
              <li class="cfg-row">
                <span class="cfg-label">CONFIRM NEW</span>
                <input class="input cfg-input" id="pw-confirm" type="password" autocomplete="new-password" minlength="8" maxlength="128" />
              </li>
            </ul>
            <p class="placeholder" style="margin-top: 14px;">
              Minimum 8 characters. Changing the admin password keeps this browser
              signed in but immediately invalidates every other active session
              for the <code>admin</code> account.
            </p>
            <p class="error" id="pw-error" hidden style="margin-top: 8px;"></p>
          </div>

          <div class="card">
            ${cardHead("Server password", "D", renderServerPasswordSaveBtn(info))}
            <p class="placeholder" style="margin-top: 0; margin-bottom: 10px;">
              Shared secret a Toki desktop client must supply on connect.
              Empty value = open mode (no password required).
            </p>
            ${cfg
              ? (info.tomlPasswordOverride
                  ? renderServerPasswordLocked()
                  : renderServerPasswordForm(cfg, state.serverPwReveal))
              : `<p class="placeholder">Loading…</p>`}
            <p class="error" id="server-pw-error" hidden style="margin-top: 8px;"></p>
          </div>
        </div>
      </div>
    `;
  }
  function cfgRow(label, value) {
    return `<li class="cfg-row"><span class="cfg-label">${esc(label)}</span><span class="cfg-value">${esc(value)}</span></li>`;
  }

  // Lazy-load server config the first time the Server section
  // renders. Cached in `state.serverConfig` thereafter; refreshed
  // implicitly after every Save (the PUT response carries the
  // normalised config). Errors surface as toasts; the form simply
  // doesn't render until the value arrives.
  async function ensureServerConfigLoaded() {
    if (state.serverConfig) return;
    try {
      state.serverConfig = await api("GET", "/api/server-config");
      render();
    } catch (e) {
      toast(`Server config load failed: ${e.message}`, "red");
    }
  }

  /** POST /api/account/password with the three form values.
   *
   * Validation happens both client- and server-side. Client-side
   * catches the trivial cases (empty fields, new !== confirm,
   * length) so the user gets immediate feedback without a network
   * round-trip; server-side is the authority and re-validates
   * everything. On success, clears the form and shows a toast that
   * mentions the side-effect on other sessions. */
  async function changePassword() {
    if (state.pwSaving) return;
    const cur = document.getElementById("pw-current");
    const nw = document.getElementById("pw-new");
    const cf = document.getElementById("pw-confirm");
    const errEl = document.getElementById("pw-error");
    if (!cur || !nw || !cf || !errEl) return;
    const showErr = (m) => {
      errEl.textContent = m;
      errEl.hidden = false;
    };
    errEl.hidden = true;

    if (!cur.value) { showErr("Current password required."); return; }
    if (nw.value.length < 8) { showErr("New password must be at least 8 characters."); return; }
    if (nw.value !== cf.value) { showErr("New password and confirmation don't match."); return; }
    if (nw.value === cur.value) { showErr("New password must differ from the current one."); return; }

    // Toggle the button via direct DOM manipulation rather than a
    // full re-render. A render() here would replace the form's
    // innerHTML — wiping the user's typed values and (on failure)
    // the error message we just set. The button state is the only
    // thing that needs to change during the save round-trip.
    state.pwSaving = true;
    const setBtn = (saving) => {
      const btn = document.querySelector("[data-pw-save]");
      if (!btn) return;
      btn.disabled = saving;
      btn.textContent = saving ? "saving…" : "Change password";
    };
    setBtn(true);
    try {
      await api("POST", "/api/account/password", {
        current: cur.value,
        new: nw.value,
      });
      // Clear the form; the cookie we hold is still valid (server
      // explicitly preserves it), so we don't need to log back in.
      // But we DO want to wipe the fields so they don't sit in the
      // DOM as cleartext for the rest of the session.
      cur.value = "";
      nw.value = "";
      cf.value = "";
      errEl.hidden = true;
      toast("Password changed (other sessions ended)");
    } catch (e) {
      showErr(e.message);
    } finally {
      state.pwSaving = false;
      setBtn(false);
    }
  }

  // ── Server password (gRPC client gate) ─────────────────────────
  //
  // Distinct from the admin password: this one is what desktop
  // clients send on Register. The admin needs to be able to read
  // the value (to share with users), so the input has a show/hide
  // toggle rather than the blind current/new/confirm flow.
  //
  // When the operator pinned the password in config.toml, the
  // server flips `tomlPasswordOverride = true` and the UI renders
  // a locked-state card instead of the form.

  function renderServerPasswordSaveBtn(info) {
    if (info && info.tomlPasswordOverride) return ""; // no button, locked
    const saving = state.serverPwSaving;
    return `<button class="btn phos sm" data-server-pw-save ${saving ? "disabled" : ""}>${saving ? "saving…" : "Save server password"}</button>`;
  }

  function renderServerPasswordLocked() {
    return `
      <ul class="cfg-list">
        <li class="cfg-row">
          <span class="cfg-label">STATUS</span>
          <span class="cfg-value" style="color: var(--amber);">Managed via <code>config.toml</code></span>
        </li>
      </ul>
      <p class="placeholder" style="margin-top: 14px;">
        Remove the <code>password = "…"</code> line from <code>config.toml</code>
        and restart the server to manage this value here.
      </p>`;
  }

  function renderServerPasswordForm(cfg, reveal) {
    const armed = (cfg.grpcPassword || "").length > 0;
    return `
      <ul class="cfg-list">
        <li class="cfg-row">
          <span class="cfg-label">STATUS</span>
          <span class="cfg-value" style="color: ${armed ? "var(--phos)" : "var(--ink-mute)"};">
            ${armed ? "ARMED" : "OPEN MODE"}
          </span>
        </li>
        <li class="cfg-row">
          <span class="cfg-label">PASSWORD</span>
          <div style="display: flex; gap: 8px; flex: 1;">
            <input class="input cfg-input"
                   id="server-pw"
                   type="${reveal ? "text" : "password"}"
                   maxlength="128"
                   value="${esc(cfg.grpcPassword || "")}"
                   placeholder="(empty = open mode)" />
            <button type="button" class="btn ghost sm" data-server-pw-reveal>${reveal ? "hide" : "show"}</button>
          </div>
        </li>
      </ul>`;
  }

  /** POST the value from the #server-pw input to the server.
   *  Same "directly toggle button state, don't re-render" pattern
   *  as changePassword — so a failed save doesn't blow away the
   *  value the user typed.
   */
  async function saveServerPassword() {
    if (state.serverPwSaving) return;
    const input = document.getElementById("server-pw");
    const errEl = document.getElementById("server-pw-error");
    if (!input || !errEl) return;
    errEl.hidden = true;

    state.serverPwSaving = true;
    const setBtn = (saving) => {
      const btn = document.querySelector("[data-server-pw-save]");
      if (!btn) return;
      btn.disabled = saving;
      btn.textContent = saving ? "saving…" : "Save server password";
    };
    setBtn(true);
    try {
      await api("PUT", "/api/server-password", { password: input.value });
      // Refresh the cached server-config so the STATUS row repaints
      // ARMED vs OPEN MODE next time the section is rendered.
      state.serverConfig = await api("GET", "/api/server-config");
      toast(
        (input.value.length > 0
          ? "Server password armed"
          : "Server password disarmed (open mode)"),
      );
      // Repaint the section once so the status pill updates. Inputs
      // outside the password input keep their values because the
      // form is driven entirely by state.serverConfig.
      renderContent();
    } catch (e) {
      errEl.textContent = e.message;
      errEl.hidden = false;
    } finally {
      state.serverPwSaving = false;
      setBtn(false);
    }
  }

  async function saveServerConfig() {
    if (state.serverConfigSaving) return;
    const sn = document.getElementById("cfg-server-name");
    const mp = document.getElementById("cfg-max-peers");
    const ik = document.getElementById("cfg-idle-kick");
    if (!sn || !mp || !ik) return;
    const body = {
      serverName: sn.value,
      maxPeers: Number(mp.value),
      idleKickSecs: Number(ik.value),
    };
    state.serverConfigSaving = true;
    render();
    try {
      state.serverConfig = await api("PUT", "/api/server-config", body);
      toast("Config saved");
    } catch (e) {
      toast(`Save failed: ${e.message}`, "red");
    } finally {
      state.serverConfigSaving = false;
      render();
    }
  }

  // ── Audit (stub) ────────────────────────────────────────────────

  function renderAudit() {
    return `
      ${sectionHead("04 · AUDIT", "Audit log", "Every administrative action and security event")}
      <div class="scroll">
        <div class="card">
          ${cardHead("Coming soon", "TODO")}
          <p class="placeholder">
            Audit logging isn't yet implemented. The intended shape
            (an append-only ring buffer of admin actions captured at
            the rename / kick / move handler boundary) is sketched in
            the design but not wired to the backend. Existing actions
            still emit structured <code>tracing</code> lines on the
            server — for now, <code>journalctl -u toki-server</code>
            (or your equivalent) is the audit trail.
          </p>
        </div>
      </div>
    `;
  }

  // ── Small composables ──────────────────────────────────────────

  function sectionHead(tag, title, desc, right = "") {
    return `
      <header class="section-head">
        <div class="left">
          <span class="tag-text">· ${esc(tag)} ·</span>
          <h2>${esc(title)}</h2>
          ${desc ? `<span class="desc">${esc(desc)}</span>` : ""}
        </div>
        <div class="right">${right}</div>
      </header>`;
  }

  function cardHead(title, tag, right = "") {
    return `
      <header class="card-head">
        <div class="left">
          ${tag ? `<span class="tag-text">· ${esc(tag)} ·</span>` : ""}
          <h4>${esc(title)}</h4>
        </div>
        ${right}
      </header>`;
  }

  // ════════════════════════════════════════════════════════════════
  // Event delegation (clicks)
  // ════════════════════════════════════════════════════════════════

  document.body.addEventListener("click", async (ev) => {
    // Every clickable element with an action belongs in this selector
    // list — `closest()` short-circuits to `null` for anything not
    // matching, so a missing entry here silently disables the button.
    // (Both `data-server-save` and `data-pw-save` were dead for this
    // reason until they got added in.)
    const target = ev.target.closest(
      "[data-section], [data-select-freq], [data-menu], [data-quick], " +
      "[data-server-save], [data-pw-save], " +
      "[data-server-pw-save], [data-server-pw-reveal]"
    );
    if (!target) return;

    // Section change
    if (target.dataset.section) {
      const s = target.dataset.section;
      if (state.section !== s) {
        state.section = s;
        renderShell();
        render();
        // Lazy load: only the Server section needs server-config;
        // we fetch it on first visit and cache for the session.
        if (s === "server") ensureServerConfigLoaded();
      }
      return;
    }
    // Server-config save button
    if (target.dataset.serverSave !== undefined) {
      saveServerConfig();
      return;
    }
    // Change-password save button
    if (target.dataset.pwSave !== undefined) {
      changePassword();
      return;
    }
    // Server-password save button (gRPC client gate)
    if (target.dataset.serverPwSave !== undefined) {
      saveServerPassword();
      return;
    }
    // Server-password reveal toggle — flips input type from password
    // to text and back. We re-render the section so the input picks
    // up the new `type` attribute and the button label flips.
    if (target.dataset.serverPwReveal !== undefined) {
      state.serverPwReveal = !state.serverPwReveal;
      renderContent();
      return;
    }
    // Channel select
    if (target.dataset.selectFreq) {
      state.selectedFreq = target.dataset.selectFreq;
      render();
      return;
    }
    // Open member action menu
    if (target.dataset.menu) {
      const peerId = target.dataset.menu;
      const anchor = target.getBoundingClientRect();
      openActionMenu(peerId, state.selectedFreq, anchor);
      return;
    }
    // Sidebar quick links
    if (target.dataset.quick === "logout") {
      logout();
      return;
    }
  });

  // Channel-filter input — separate handler so we don't re-render
  // the whole channel list on every keystroke from anywhere else.
  document.body.addEventListener("input", (ev) => {
    if (ev.target.id === "chan-filter") {
      state.filter = ev.target.value;
      // render() preserves focus + selection generically, so we
      // don't need the explicit refocus dance any more.
      render();
      return;
    }
    // Server-config form fields — mirror DOM value into state so a
    // 1Hz SSE re-render doesn't blow away pending edits. We do NOT
    // POST on each keystroke; the user explicitly clicks Save.
    if (state.serverConfig && ev.target.id?.startsWith("cfg-")) {
      const cfg = state.serverConfig;
      switch (ev.target.id) {
        case "cfg-server-name":
          cfg.serverName = ev.target.value;
          break;
        case "cfg-max-peers":
          cfg.maxPeers = Number(ev.target.value);
          break;
        case "cfg-idle-kick":
          cfg.idleKickSecs = Number(ev.target.value);
          break;
      }
    }
    // Server password input — mirror into state so the reveal
    // toggle (which re-renders) doesn't reset the user's typed
    // value back to whatever's currently in the db.
    if (state.serverConfig && ev.target.id === "server-pw") {
      state.serverConfig.grpcPassword = ev.target.value;
    }
  });

  // "Active only" checkbox — toggles the empty-channel filter on the
  // channels list. When turning it on, the previously-selected
  // channel may no longer be visible (operator was viewing an empty
  // freq); we snap the selection to the first active room so the
  // detail panel doesn't show a channel the operator just hid.
  document.body.addEventListener("change", (ev) => {
    if (ev.target.id === "chan-active-only") {
      state.activeOnly = ev.target.checked;
      if (state.activeOnly && state.snap) {
        const stillVisible = state.snap.rooms.some(
          (r) => r.frequency === state.selectedFreq
        );
        if (!stillVisible) {
          // Fall back to the first active freq, or null if none —
          // the detail panel handles `selectedFreq === null` by
          // showing the channel-level empty state.
          state.selectedFreq = state.snap.rooms[0]?.frequency || null;
        }
      }
      render();
    }
  });

  // Logout via topbar button
  $("#logout").addEventListener("click", logout);

  // ════════════════════════════════════════════════════════════════
  // Action menu (per-member popover) + mutation modals
  // ════════════════════════════════════════════════════════════════

  let menuEl = null;
  function closeActionMenu() {
    if (menuEl) { menuEl.remove(); menuEl = null; }
  }
  document.body.addEventListener("click", (ev) => {
    if (menuEl && !ev.target.closest(".action-menu") && !ev.target.closest("[data-menu]")) {
      closeActionMenu();
    }
  }, true);

  function openActionMenu(peerId, freq, anchor) {
    closeActionMenu();
    const snap = state.snap;
    if (!snap) return;
    const room = snap.rooms.find((r) => r.frequency === freq);
    const peer = room?.members.find((m) => m.id === peerId);
    if (!peer) return;

    menuEl = document.createElement("div");
    menuEl.className = "action-menu";
    menuEl.style.top  = `${anchor.bottom + 6}px`;
    menuEl.style.left = `${Math.max(12, anchor.right - 240)}px`;
    menuEl.innerHTML = renderMenuRoot(peer);

    menuEl.addEventListener("click", (e) => {
      const btn = e.target.closest("button[data-action]");
      if (!btn) return;
      const act = btn.dataset.action;
      if (act === "move-open") {
        menuEl.innerHTML = renderMenuMoveSub(peer, freq);
        return;
      }
      if (act === "move-back") {
        menuEl.innerHTML = renderMenuRoot(peer);
        return;
      }
      if (act === "move-to") {
        const target = btn.dataset.freq;
        closeActionMenu();
        doMove(peer, target);
        return;
      }
      if (act === "rename") {
        closeActionMenu();
        promptRename(peer);
        return;
      }
      if (act === "kick") {
        closeActionMenu();
        confirmKick(peer);
        return;
      }
    });

    document.body.appendChild(menuEl);
  }

  function renderMenuRoot(peer) {
    return `
      <div class="head">
        <span class="label">ACT ON</span>
        <span class="name">${esc(peer.displayName)}</span>
      </div>
      <button data-action="rename"><span class="icon">✎</span><span>Rename callsign</span></button>
      <button data-action="move-open"><span class="icon">⇄</span><span>Move to channel…</span><span style="margin-left:auto; color: var(--ink-faint);">›</span></button>
      <button class="amber" data-action="kick"><span class="icon">↯</span><span>Kick (disconnect)</span></button>
      <button disabled title="Not implemented — no per-peer mod system yet"><span class="icon">★</span><span>Promote to mod</span></button>
      <button disabled title="Not implemented — no DM/whisper backend"><span class="icon">~</span><span>Whisper…</span></button>
      <button disabled class="red" title="Not implemented — kick already force-disconnects"><span class="icon">⊘</span><span>Force-disconnect</span></button>
    `;
  }

  function renderMenuMoveSub(peer, currentFreq) {
    const opts = ALL_FREQS.filter((f) => f !== currentFreq).map((f) => `
      <button data-action="move-to" data-freq="${esc(f)}">
        <span class="icon" style="font-size: 10px; color: var(--ink-faint); width: 24px;">${String(chanNumOf(f)).padStart(2, "0")}</span>
        <span style="font-family: var(--font-mono); font-size: 12px; letter-spacing: 0.04em; font-variant-numeric: tabular-nums;">
          ${esc(f)} <span style="margin-left:6px; color: var(--ink-faint); font-size: 9px; letter-spacing: 0.18em;">MHz</span>
        </span>
      </button>`).join("");
    return `
      <div class="submenu-head">
        <button class="back" data-action="move-back">‹ back</button>
        <span style="margin-left: auto;">MOVE TO</span>
      </div>
      <div class="submenu">${opts}</div>
    `;
  }

  // ── Mutations ─────────────────────────────────────────────────

  async function doMove(peer, target) {
    try {
      await api("POST", `/api/clients/${encodeURIComponent(peer.id)}/move`, { frequency: target });
      toast(`Moved ${peer.displayName} → ${target}`);
    } catch (e) {
      toast(`Move failed: ${e.message}`, "red");
    }
  }

  function confirmKick(peer) {
    if (!confirm(`Kick ${peer.displayName}? They'll be disconnected immediately.`)) return;
    api("POST", `/api/clients/${encodeURIComponent(peer.id)}/kick`)
      .then(() => toast(`Kicked ${peer.displayName}`, "amber"))
      .catch((e) => toast(`Kick failed: ${e.message}`, "red"));
  }

  function promptRename(peer) {
    const dlg = ensureModal();
    dlg.querySelector("[data-modal-tag]").textContent = "RENAME";
    dlg.querySelector("[data-modal-title]").textContent = `Rename ${peer.displayName}`;
    dlg.querySelector("[data-modal-label]").textContent = "NEW CALLSIGN";
    const input = dlg.querySelector("[data-modal-input]");
    input.value = peer.displayName;
    input.maxLength = 32;
    const onCancel = () => dlg.close("cancel");
    dlg.querySelector("[data-modal-cancel]").onclick = onCancel;
    dlg.querySelector("[data-modal-close]").onclick = onCancel;
    dlg.onclose = async () => {
      if (dlg.returnValue !== "ok") return;
      const val = input.value.trim();
      if (!val) return;
      try {
        await api("POST", `/api/clients/${encodeURIComponent(peer.id)}/rename`, { displayName: val });
        toast(`Renamed → ${val}`);
      } catch (e) {
        toast(`Rename failed: ${e.message}`, "red");
      }
    };
    dlg.showModal();
    input.focus();
    input.select();
  }

  function ensureModal() {
    let dlg = document.getElementById("admin-modal");
    if (dlg) return dlg;
    dlg = document.createElement("dialog");
    dlg.id = "admin-modal";
    dlg.className = "modal";
    // The cancel button is `type="button"` so it does NOT submit the
    // form — Enter falls through to OK, which is the only submit
    // button and trips method="dialog" → returnValue="ok".
    dlg.innerHTML = `
      <header class="head">
        <span class="tag-text" data-modal-tag>—</span>
        <h3 data-modal-title>—</h3>
        <button type="button" class="close" data-modal-close>×</button>
      </header>
      <form method="dialog">
        <label>
          <span data-modal-label>VALUE</span>
          <input class="input" type="text" data-modal-input required />
        </label>
        <menu>
          <button type="button" class="btn ghost" data-modal-cancel>Cancel</button>
          <button class="btn phos" value="ok">OK</button>
        </menu>
      </form>`;
    document.body.appendChild(dlg);
    return dlg;
  }

  // ════════════════════════════════════════════════════════════════
  // Go
  // ════════════════════════════════════════════════════════════════

  start();
})();
