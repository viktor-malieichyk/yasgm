const { invoke } = window.__TAURI__.core;

const MODES = ["auto", "sync", "backup", "off"];

let gamesListEl;
let detailEmptyEl;
let detailContentEl;
let detailTitleEl;
let detailAppIdEl;
let modeSelectEl;
let keepInputEl;
let saveBtnEl;
let resetBtnEl;
let versionsListEl;
let statusMsgEl;

let games = [];
let selectedAppId = null;

function setStatus(text) {
  statusMsgEl.textContent = text;
}

function humanBytes(bytes) {
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

function humanDate(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

// ---- games sidebar ----------------------------------------------------

async function loadGames() {
  try {
    games = await invoke("list_games");
  } catch (err) {
    setStatus(`error loading games: ${err}`);
    return;
  }
  renderGamesList();
  if (selectedAppId === null && games.length > 0) {
    selectGame(games[0].app_id);
  } else if (selectedAppId !== null && games.some((g) => g.app_id === selectedAppId)) {
    renderDetailSettings();
  } else {
    selectedAppId = null;
    showEmptyDetail();
  }
}

function renderGamesList() {
  gamesListEl.innerHTML = "";
  for (const g of games) {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "game-item" + (g.app_id === selectedAppId ? " selected" : "");
    item.innerHTML = `<span class="game-name">${g.game}</span><span class="muted">${g.app_id}</span>`;
    item.addEventListener("click", () => selectGame(g.app_id));
    gamesListEl.appendChild(item);
  }
}

function selectGame(appId) {
  selectedAppId = appId;
  renderGamesList();
  renderDetailSettings();
  loadVersions();
}

function showEmptyDetail() {
  detailEmptyEl.classList.remove("hidden");
  detailContentEl.classList.add("hidden");
}

function renderDetailSettings() {
  const g = games.find((game) => game.app_id === selectedAppId);
  if (!g) {
    showEmptyDetail();
    return;
  }
  detailEmptyEl.classList.add("hidden");
  detailContentEl.classList.remove("hidden");

  detailTitleEl.textContent = g.game;
  detailAppIdEl.textContent = `AppID ${g.app_id} · ${g.effective_mode}`;

  modeSelectEl.innerHTML = "";
  for (const m of MODES) {
    const opt = document.createElement("option");
    opt.value = m;
    opt.textContent = m;
    if (m === g.mode) opt.selected = true;
    modeSelectEl.appendChild(opt);
  }
  keepInputEl.placeholder = `default (${g.default_keep})`;
  keepInputEl.value = g.keep !== null && g.keep !== undefined ? g.keep : "";
}

async function saveSelectedGame() {
  if (selectedAppId === null) return;
  setStatus("saving…");
  try {
    const keep = keepInputEl.value === "" ? null : Number(keepInputEl.value);
    const result = await invoke("set_game_config", {
      appId: selectedAppId,
      mode: modeSelectEl.value,
      keep,
    });
    setStatus(result.trim() || "saved");
    await loadGames();
  } catch (err) {
    setStatus(`save failed: ${err}`);
  }
}

async function resetSelectedGame() {
  if (selectedAppId === null) return;
  setStatus("resetting…");
  try {
    const result = await invoke("clear_game_config", { appId: selectedAppId });
    setStatus(result.trim() || "reset");
    await loadGames();
  } catch (err) {
    setStatus(`reset failed: ${err}`);
  }
}

// ---- versions for the selected game ------------------------------------

async function loadVersions() {
  if (selectedAppId === null) return;
  versionsListEl.innerHTML = "";
  try {
    const versions = await invoke("list_versions", { appId: selectedAppId });
    if (versions.length === 0) {
      versionsListEl.innerHTML = '<p class="muted">no versions yet</p>';
      return;
    }
    versions.sort((a, b) => b.created - a.created);
    for (const v of versions) {
      versionsListEl.appendChild(renderVersionRow(v));
    }
  } catch (err) {
    setStatus(`error loading versions: ${err}`);
  }
}

function renderVersionRow(v) {
  const row = document.createElement("div");
  row.className = "version-row";

  const info = document.createElement("div");
  info.className = "version-info";
  const badge = v.active ? '<span class="badge active">active</span>' : v.pinned ? '<span class="badge pinned">pinned</span>' : "";
  info.innerHTML = `
    <div>${humanDate(v.created)} ${badge}</div>
    <div class="muted">${v.machine} · ${v.os} · ${v.files} files · ${humanBytes(v.size)}</div>
  `;
  row.appendChild(info);

  const actions = document.createElement("div");
  actions.className = "version-actions";

  const restoreBtn = document.createElement("button");
  restoreBtn.textContent = "Restore";
  restoreBtn.disabled = v.active;
  restoreBtn.addEventListener("click", async () => {
    setStatus(`restoring ${v.id}…`);
    try {
      const result = await invoke("restore_version", { appId: v.app_id, versionId: v.id });
      setStatus(result.trim() || "restored");
      await loadVersions();
    } catch (err) {
      setStatus(`restore failed: ${err}`);
    }
  });
  actions.appendChild(restoreBtn);

  const pinBtn = document.createElement("button");
  pinBtn.textContent = v.pinned ? "Unpin" : "Pin";
  pinBtn.addEventListener("click", async () => {
    setStatus(`${v.pinned ? "unpinning" : "pinning"} ${v.id}…`);
    try {
      const result = await invoke("set_pinned", {
        appId: v.app_id,
        versionId: v.id,
        pinned: !v.pinned,
      });
      setStatus(result.trim() || "done");
      await loadVersions();
    } catch (err) {
      setStatus(`pin toggle failed: ${err}`);
    }
  });
  actions.appendChild(pinBtn);

  const deleteBtn = document.createElement("button");
  deleteBtn.textContent = "Delete";
  // Guard rail, not a D5/D14 requirement: avoids a stray click deleting the
  // version you're currently restoring from.
  deleteBtn.disabled = v.active;
  deleteBtn.addEventListener("click", async () => {
    setStatus(`deleting ${v.id}…`);
    try {
      const result = await invoke("remove_version", { appId: v.app_id, versionId: v.id });
      setStatus(result.trim() || "deleted");
      await loadVersions();
    } catch (err) {
      setStatus(`delete failed: ${err}`);
    }
  });
  actions.appendChild(deleteBtn);

  row.appendChild(actions);
  return row;
}

window.addEventListener("DOMContentLoaded", () => {
  gamesListEl = document.querySelector("#games-list");
  detailEmptyEl = document.querySelector("#detail-empty");
  detailContentEl = document.querySelector("#detail-content");
  detailTitleEl = document.querySelector("#detail-title");
  detailAppIdEl = document.querySelector("#detail-appid");
  modeSelectEl = document.querySelector("#mode-select");
  keepInputEl = document.querySelector("#keep-input");
  saveBtnEl = document.querySelector("#save-btn");
  resetBtnEl = document.querySelector("#reset-btn");
  versionsListEl = document.querySelector("#versions-list");
  statusMsgEl = document.querySelector("#status-msg");

  saveBtnEl.addEventListener("click", saveSelectedGame);
  resetBtnEl.addEventListener("click", resetSelectedGame);

  loadGames();
});
