// Toki admin panel — vanilla JS, no build step, no dependencies.
//
// The shape contract is tied to `dto::Snapshot` in the Rust source:
//   { rooms: [{frequency, holder, members: [{id, displayName, lastSeenSecs}]}],
//     lobby: [{id, displayName, lastSeenSecs}],
//     generation: number }
//
// Every SSE message is a full snapshot; we replace the DOM each tick.
// This is wasteful in theory and fine in practice — a busy server has
// ~tens of clients, so the diff is dominated by JSON parsing anyway.

(() => {
  const $ = (sel) => document.querySelector(sel);
  const main = $("#main");
  const statusEl = $("#status");
  const logoutBtn = $("#logout");
  const eventLog = $("#event-log");
  const roomsGrid = $("#rooms-grid");
  const lobbyList = $("#lobby-list");

  let evtSource = null;

  // ---- view switching ----------------------------------------------
  function show(view) {
    main.dataset.view = view;
    logoutBtn.hidden = view !== "dashboard";
  }

  // ---- HTTP helpers ------------------------------------------------
  // We rely on the browser to attach the session cookie automatically;
  // never include it in JS land (it's HttpOnly anyway).
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

  // ---- bootstrap ---------------------------------------------------
  async function start() {
    show("loading");
    try {
      const snap = await api("GET", "/api/state");
      enterDashboard(snap);
    } catch (e) {
      // 401 → render the login form; anything else → still show the
      // login form with the error, so the operator at least gets a UI.
      show("login");
      if (!String(e.message).startsWith("401")) {
        $("#login-error").textContent = e.message;
        $("#login-error").hidden = false;
      }
    }
  }

  // ---- login -------------------------------------------------------
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

  logoutBtn.addEventListener("click", async () => {
    try {
      await api("POST", "/api/logout");
    } catch {}
    if (evtSource) { evtSource.close(); evtSource = null; }
    show("login");
    statusEl.textContent = "";
  });

  // ---- dashboard ---------------------------------------------------
  function enterDashboard(initial) {
    show("dashboard");
    render(initial);
    openEventStream();
  }

  function openEventStream() {
    if (evtSource) evtSource.close();
    evtSource = new EventSource("/api/events");
    evtSource.addEventListener("state", (e) => {
      try {
        const snap = JSON.parse(e.data);
        render(snap);
        setStatus("ok", "live feed connected");
      } catch (err) {
        appendLog(`parse error: ${err.message}`);
      }
    });
    evtSource.addEventListener("error", () => {
      // EventSource auto-reconnects on its own; we just surface the
      // status to the operator so they know the feed is degraded.
      setStatus("bad", "live feed reconnecting…");
    });
  }

  // Toggle the dot's color via class; the `title` attribute exposes
  // the human-readable status on hover so we don't need a label.
  function setStatus(cls, label) {
    statusEl.className = `status ${cls}`;
    statusEl.title = label;
    statusEl.setAttribute("aria-label", label);
  }

  // ---- rendering ---------------------------------------------------
  function render(snap) {
    renderRooms(snap.rooms || []);
    renderLobby(snap.lobby || []);
  }

  function renderRooms(rooms) {
    roomsGrid.replaceChildren();
    if (rooms.length === 0) {
      const empty = document.createElement("p");
      empty.className = "hint";
      empty.textContent = "No active frequencies.";
      roomsGrid.append(empty);
      return;
    }
    for (const room of rooms) {
      roomsGrid.append(renderRoom(room));
    }
  }

  function renderRoom(room) {
    const card = document.createElement("article");
    card.className = "room";
    const head = document.createElement("header");
    const h3 = document.createElement("h3");
    h3.textContent = room.frequency + " MHz";
    const count = document.createElement("span");
    count.className = "count";
    count.textContent = `${room.members.length} member${room.members.length === 1 ? "" : "s"}`;
    head.append(h3, count);
    card.append(head);

    const ul = document.createElement("ul");
    for (const m of room.members) {
      ul.append(renderMember(m, room.holder === m.id));
    }
    card.append(ul);
    return card;
  }

  function renderMember(member, isHolder) {
    const li = document.createElement("li");
    if (isHolder) li.classList.add("holder");

    const name = document.createElement("span");
    name.className = "member-name";
    name.textContent = member.displayName;
    li.append(name);

    const meta = document.createElement("span");
    meta.className = "member-meta";
    meta.textContent = `${member.lastSeenSecs}s`;
    li.append(meta);

    const actions = document.createElement("span");
    actions.className = "actions";
    actions.append(
      mkActionButton("move", () => promptMove(member)),
      mkActionButton("rename", () => promptRename(member)),
      mkActionButton("kick", () => doKick(member), true),
    );
    li.append(actions);
    return li;
  }

  function mkActionButton(label, onClick, danger) {
    const b = document.createElement("button");
    b.textContent = label;
    if (danger) b.className = "danger";
    b.addEventListener("click", onClick);
    return b;
  }

  function renderLobby(lobby) {
    lobbyList.replaceChildren();
    for (const m of lobby) {
      const li = document.createElement("li");
      li.textContent = `${m.displayName} (${m.lastSeenSecs}s)`;
      lobbyList.append(li);
    }
  }

  // ---- mutations ---------------------------------------------------
  async function doKick(member) {
    if (!confirm(`Kick ${member.displayName}?`)) return;
    try {
      await api("POST", `/api/clients/${encodeURIComponent(member.id)}/kick`);
      appendLog(`kicked ${member.displayName}`);
    } catch (e) {
      appendLog(`kick failed: ${e.message}`);
    }
  }

  async function promptMove(member) {
    const freq = await openPrompt({
      title: `Move ${member.displayName}`,
      label: "frequency (e.g. 446.05)",
      initial: "",
    });
    if (freq == null) return;
    try {
      await api("POST", `/api/clients/${encodeURIComponent(member.id)}/move`, { frequency: freq });
      appendLog(`moved ${member.displayName} → ${freq}`);
    } catch (e) {
      appendLog(`move failed: ${e.message}`);
    }
  }

  async function promptRename(member) {
    const name = await openPrompt({
      title: `Rename ${member.displayName}`,
      label: "new callsign",
      initial: member.displayName,
    });
    if (name == null) return;
    try {
      await api("POST", `/api/clients/${encodeURIComponent(member.id)}/rename`, { displayName: name });
      appendLog(`renamed ${member.displayName} → ${name}`);
    } catch (e) {
      appendLog(`rename failed: ${e.message}`);
    }
  }

  // ---- modal -------------------------------------------------------
  // <dialog> + a Promise resolves on close. Resolves to the input
  // value on "ok", or null on cancel — caller checks for null.
  //
  // The OK button uses the form's native `method="dialog"` submit
  // path: pressing Enter (or clicking OK) submits the form, which
  // closes the dialog with returnValue="ok". The required input
  // gives us empty-value validation for free.
  //
  // The cancel button is NOT a submit (markup uses type="button"),
  // so we close the dialog explicitly here with returnValue="cancel".
  // That sidesteps the required-input constraint that would
  // otherwise block a cancel-submit when the input is empty.
  function openPrompt({ title, label, initial }) {
    const dlg = $("#prompt-modal");
    $("#prompt-title").textContent = title;
    $("#prompt-label").textContent = label;
    const input = $("#prompt-input");
    input.value = initial ?? "";
    $("#prompt-error").hidden = true;

    const cancelBtn = $("#prompt-cancel");
    const onCancel = () => dlg.close("cancel");
    cancelBtn.addEventListener("click", onCancel);

    return new Promise((resolve) => {
      const onClose = () => {
        dlg.removeEventListener("close", onClose);
        cancelBtn.removeEventListener("click", onCancel);
        const val = dlg.returnValue === "ok" ? input.value.trim() : null;
        resolve(val);
      };
      dlg.addEventListener("close", onClose);
      dlg.showModal();
      input.focus();
      input.select();
    });
  }

  // ---- event log ---------------------------------------------------
  function appendLog(msg) {
    const li = document.createElement("li");
    li.textContent = `${new Date().toLocaleTimeString()} · ${msg}`;
    li.classList.add("fresh");
    eventLog.prepend(li);
    // Cap the log so a long admin session doesn't grow unbounded.
    while (eventLog.children.length > 20) eventLog.lastChild.remove();
  }

  // ---- go ----------------------------------------------------------
  start();
})();
