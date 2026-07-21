const { invoke } = window.__TAURI__.core;

const MODES = ["auto", "sync", "backup", "off"];
const THEME_KEY = "yasgm-theme";

let gamesListEl;
let detailEmptyEl;
let detailContentEl;
let detailTitleEl;
let detailAppIdEl;
let modeSelectEl;
let keepInputEl;
let saveBtnEl;
let resetBtnEl;
let openSavesBtnEl;
let openBackupsBtnEl;
let openLocationMsgEl;
let versionsListEl;
let statusMsgEl;
let themeSwitchEl;
let navLibraryEl;
let navSettingsEl;
let libraryViewEl;
let settingsViewEl;
let providerCurrentEl;
let providerCheckBtnEl;
let providerStatusMsgEl;
let providerOnedriveBtnEl;
let providerAuthBtnEl;
let providerAuthMsgEl;
let providerLocalPathEl;
let providerLocalBtnEl;
let autostartToggleEl;
let autostartDetailEl;

let games = [];
let selectedAppId = null;

// ---- top-level view switching (Library / Settings) -----------------------

function showView(view) {
  const isLibrary = view === "library";
  libraryViewEl.classList.toggle("hidden", !isLibrary);
  settingsViewEl.classList.toggle("hidden", isLibrary);
  navLibraryEl.classList.toggle("selected", isLibrary);
  navSettingsEl.classList.toggle("selected", !isLibrary);
  if (!isLibrary) {
    loadProvider();
    loadAutostart();
  }
}

// ---- theme (light/dark/system) ------------------------------------------

function applyTheme(theme) {
  if (theme === "system") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = theme;
  }
  for (const seg of themeSwitchEl.querySelectorAll(".segment")) {
    seg.classList.toggle("selected", seg.dataset.themeOption === theme);
  }
  // Also sync the native window chrome (titlebar) where available; the
  // CSS above is what actually themes the page content either way.
  const win = window.__TAURI__?.window?.getCurrentWindow?.();
  if (win) {
    win.setTheme(theme === "system" ? null : theme).catch(() => {});
  }
}

function loadTheme() {
  applyTheme(localStorage.getItem(THEME_KEY) || "system");
}

async function loadAccentColor() {
  try {
    const { accent, text } = await invoke("get_accent_color");
    document.documentElement.style.setProperty("--accent", accent);
    document.documentElement.style.setProperty("--accent-text", text);
  } catch {
    // Static CSS fallback (--accent in styles.css) stays in effect.
  }
}

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
  openLocationMsgEl.textContent = "";
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

async function openSaveLocation() {
  if (selectedAppId === null) return;
  openLocationMsgEl.textContent = "looking for save location…";
  try {
    const paths = await invoke("get_save_paths", { appId: selectedAppId });
    if (paths.length === 0) {
      openLocationMsgEl.textContent = "no save data found on this machine yet";
      return;
    }
    // Most games resolve to a single root; if more than one, just open the
    // first — the rest are still visible via `yasgm doctor`.
    await invoke("open_path", { path: paths[0].path });
    openLocationMsgEl.textContent = "";
  } catch (err) {
    openLocationMsgEl.textContent = `couldn't open save location: ${err}`;
  }
}

async function openBackupsLocation() {
  if (selectedAppId === null) return;
  openLocationMsgEl.textContent = "looking for backups location…";
  try {
    const loc = await invoke("get_backup_location", { appId: selectedAppId });
    if (loc.kind === "cloud") {
      openLocationMsgEl.textContent =
        "backups are stored in OneDrive (cloud) — no local folder to open";
      return;
    }
    await invoke("open_path", { path: loc.path });
    openLocationMsgEl.textContent = "";
  } catch (err) {
    openLocationMsgEl.textContent = `couldn't open backups location: ${err}`;
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

// ---- settings: cloud provider ---------------------------------------------

function describeProvider(p) {
  return p.type === "onedrive" ? "OneDrive" : `Local folder — ${p.path}`;
}

async function loadProvider() {
  try {
    const p = await invoke("get_provider");
    providerCurrentEl.textContent = `Current provider: ${describeProvider(p)}`;
    providerLocalPathEl.value = p.type === "local" ? p.path : "";
  } catch (err) {
    providerCurrentEl.textContent = `error loading provider: ${err}`;
  }
}

async function checkProviderStatus() {
  providerStatusMsgEl.textContent = "checking…";
  try {
    const result = await invoke("cloud_status");
    providerStatusMsgEl.textContent = result.trim();
  } catch (err) {
    providerStatusMsgEl.textContent = `not ready: ${err}`;
  }
}

async function useOnedrive() {
  setStatus("switching to OneDrive…");
  try {
    const result = await invoke("set_provider_onedrive");
    setStatus(result.trim() || "switched to OneDrive");
    await loadProvider();
  } catch (err) {
    setStatus(`switch failed: ${err}`);
  }
}

async function signInOnedrive() {
  providerAuthMsgEl.textContent = "opening browser for sign-in…";
  providerAuthBtnEl.disabled = true;
  try {
    const result = await invoke("cloud_auth");
    providerAuthMsgEl.textContent = result.trim().split("\n")[0] || "signed in";
  } catch (err) {
    providerAuthMsgEl.textContent = `sign-in failed: ${err}`;
  } finally {
    providerAuthBtnEl.disabled = false;
  }
}

async function useLocalFolder() {
  const path = providerLocalPathEl.value.trim();
  if (!path) {
    setStatus("enter a folder path first");
    return;
  }
  setStatus(`switching to local folder ${path}…`);
  try {
    const result = await invoke("set_provider_local", { path });
    setStatus(result.trim() || "switched to local folder");
    await loadProvider();
  } catch (err) {
    setStatus(`switch failed: ${err}`);
  }
}

// ---- settings: autostart --------------------------------------------------

async function loadAutostart() {
  try {
    const a = await invoke("get_autostart");
    autostartToggleEl.checked = a.enabled;
    autostartDetailEl.textContent = a.detail ? a.detail : "";
  } catch (err) {
    autostartDetailEl.textContent = `error loading autostart: ${err}`;
  }
}

async function toggleAutostart() {
  const enabled = autostartToggleEl.checked;
  setStatus(enabled ? "enabling autostart…" : "disabling autostart…");
  try {
    const result = await invoke("set_autostart", { enabled });
    setStatus(result.trim() || "done");
    await loadAutostart();
  } catch (err) {
    setStatus(`autostart change failed: ${err}`);
    autostartToggleEl.checked = !enabled;
  }
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
  openSavesBtnEl = document.querySelector("#open-saves-btn");
  openBackupsBtnEl = document.querySelector("#open-backups-btn");
  openLocationMsgEl = document.querySelector("#open-location-msg");
  versionsListEl = document.querySelector("#versions-list");
  statusMsgEl = document.querySelector("#status-msg");
  themeSwitchEl = document.querySelector("#theme-switch");
  navLibraryEl = document.querySelector("#nav-library");
  navSettingsEl = document.querySelector("#nav-settings");
  libraryViewEl = document.querySelector("#library-view");
  settingsViewEl = document.querySelector("#settings-view");
  providerCurrentEl = document.querySelector("#provider-current");
  providerCheckBtnEl = document.querySelector("#provider-check-btn");
  providerStatusMsgEl = document.querySelector("#provider-status-msg");
  providerOnedriveBtnEl = document.querySelector("#provider-onedrive-btn");
  providerAuthBtnEl = document.querySelector("#provider-auth-btn");
  providerAuthMsgEl = document.querySelector("#provider-auth-msg");
  providerLocalPathEl = document.querySelector("#provider-local-path");
  providerLocalBtnEl = document.querySelector("#provider-local-btn");
  autostartToggleEl = document.querySelector("#autostart-toggle");
  autostartDetailEl = document.querySelector("#autostart-detail");

  saveBtnEl.addEventListener("click", saveSelectedGame);
  resetBtnEl.addEventListener("click", resetSelectedGame);
  openSavesBtnEl.addEventListener("click", openSaveLocation);
  openBackupsBtnEl.addEventListener("click", openBackupsLocation);

  navLibraryEl.addEventListener("click", () => showView("library"));
  navSettingsEl.addEventListener("click", () => showView("settings"));

  for (const seg of themeSwitchEl.querySelectorAll(".segment")) {
    seg.addEventListener("click", () => {
      const theme = seg.dataset.themeOption;
      localStorage.setItem(THEME_KEY, theme);
      applyTheme(theme);
    });
  }

  providerCheckBtnEl.addEventListener("click", checkProviderStatus);
  providerOnedriveBtnEl.addEventListener("click", useOnedrive);
  providerAuthBtnEl.addEventListener("click", signInOnedrive);
  providerLocalBtnEl.addEventListener("click", useLocalFolder);
  autostartToggleEl.addEventListener("change", toggleAutostart);

  loadTheme();
  loadAccentColor();
  loadGames();
});
