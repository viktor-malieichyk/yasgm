const { invoke } = window.__TAURI__.core;

const MODES = ["auto", "sync", "backup", "off"];
const THEME_KEY = "yasgm-theme";
const AUTO_UPDATE_KEY = "yasgm-auto-update";
const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

let gamesListEl;
let detailEmptyEl;
let detailContentEl;
let detailTitleEl;
let detailAppIdEl;
let detailWarningEl;
let modeSelectEl;
let keepInputEl;
let saveBtnEl;
let resetBtnEl;
let openSavesBtnEl;
let openProviderBtnEl;
let openBackupsBtnEl;
let openLocationMsgEl;
let versionsListEl;
let versionsSpinnerEl;
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
let providerLocalBrowseBtnEl;
let providerLocalBtnEl;
let autostartToggleEl;
let autostartDetailEl;
let updateBannerEl;
let updateBannerTextEl;
let updateInstallBtnEl;
let updateCurrentEl;
let updateAutoToggleEl;
let updateCheckBtnEl;
let updateMsgEl;

let games = [];
let selectedAppId = null;
// appid → data: URL of the game's icon (see loadIcons).
let icons = {};

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

// ---- theme (system/light/dark/steam) -------------------------------------

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
    const native = theme === "system" ? null : theme === "steam" ? "dark" : theme;
    win.setTheme(native).catch(() => {});
  }
}

function loadTheme() {
  applyTheme(localStorage.getItem(THEME_KEY) || "system");
}

async function loadAccentColor() {
  try {
    const { accent, text } = await invoke("get_accent_color");
    // --system-*, not --accent directly: Light/Dark derive --accent from
    // these, while the Steam theme keeps its own blue.
    document.documentElement.style.setProperty("--system-accent", accent);
    document.documentElement.style.setProperty("--system-accent-text", text);
  } catch {
    // Static CSS fallback (in styles.css) stays in effect.
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

// Icons come in two passes: Steam's local cache first (instant), then a
// second pass that downloads whatever Steam hadn't cached.
async function loadIcons() {
  for (const offline of [true, false]) {
    try {
      const found = await invoke("game_icons", { offline });
      const changed = Object.keys(found).some((id) => icons[id] !== found[id]);
      icons = { ...icons, ...found };
      if (changed) {
        renderGamesList();
        renderDetailIcon();
      }
    } catch (err) {
      console.warn("loading game icons failed:", err);
    }
  }
}

// Compact sidebar label for `effective_mode` (the full text stays in the
// detail header): "backup-only (auto: Steam Cloud)" → "Backup only · Steam Cloud".
function shortMode(g) {
  if (!g.in_manifest) return "No known save location";
  const m = g.effective_mode.match(/^([\w-]+)(?: \((.*)\))?$/);
  if (!m) return g.effective_mode;
  const mode = m[1].replace("-", " ");
  const label = mode[0].toUpperCase() + mode.slice(1);
  const why = m[2] === "override" ? "set manually" : (m[2] || "").replace(/^auto:?\s*/, "");
  return why ? `${label} · ${why}` : label;
}

// An <img> for the game's icon, or a letter tile in a per-game color.
function gameIcon(g, large = false) {
  const url = icons[g.app_id];
  const el = document.createElement(url ? "img" : "span");
  el.className = "game-icon" + (large ? " large" : "");
  if (url) {
    el.src = url;
    el.alt = "";
  } else {
    el.textContent = (g.game.match(/[\p{L}\p{N}]/u) || ["?"])[0].toUpperCase();
    el.style.setProperty("--hue", (g.app_id * 47) % 360);
  }
  return el;
}

function renderGamesList() {
  gamesListEl.innerHTML = "";
  for (const g of games) {
    const item = document.createElement("button");
    item.type = "button";
    item.className =
      "game-item" + (g.app_id === selectedAppId ? " selected" : "") + (g.in_manifest ? "" : " unsupported");
    item.title = g.in_manifest ? g.game : `${g.game} — no save locations known for this game`;
    const text = document.createElement("span");
    text.className = "game-text";
    const name = document.createElement("span");
    name.className = "game-name";
    name.textContent = g.game;
    const sub = document.createElement("span");
    sub.className = "game-sub";
    sub.textContent = shortMode(g);
    text.append(name, sub);
    item.append(gameIcon(g), text);
    item.addEventListener("click", () => {
      showView("library");
      selectGame(g.app_id);
    });
    gamesListEl.appendChild(item);
  }
}

function renderDetailIcon() {
  const g = games.find((game) => game.app_id === selectedAppId);
  const slot = document.querySelector("#detail-icon");
  if (!g || !slot) return;
  const icon = gameIcon(g, true);
  icon.id = "detail-icon";
  slot.replaceWith(icon);
}

function selectGame(appId) {
  selectedAppId = appId;
  renderGamesList();
  renderDetailSettings();
  openLocationMsgEl.textContent = "";
  loadVersions(true);
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
  renderDetailIcon();
  detailAppIdEl.textContent = `AppID ${g.app_id} · ${g.effective_mode}`;
  // Set by the CLI when this game's save locations can't be mapped between
  // this OS and another (see resolve::cross_os_gaps).
  detailWarningEl.textContent = g.cross_os_note ? `Cross-platform restore: ${g.cross_os_note}.` : "";
  detailWarningEl.classList.toggle("hidden", !g.cross_os_note);

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

// Opens wherever backups actually live on the *primary* provider: a local
// folder for LocalFolder, or a real OneDrive web link for OneDrive (falls
// back to the app's OneDrive root if this game has no backups yet).
async function openProviderLocation() {
  if (selectedAppId === null) return;
  openLocationMsgEl.textContent = "looking for cloud provider location…";
  try {
    const loc = await invoke("get_backup_location", { appId: selectedAppId });
    if (loc.kind === "cloud") {
      if (!loc.url) {
        openLocationMsgEl.textContent = "couldn't find a OneDrive link for this game";
        return;
      }
      await invoke("open_url", { url: loc.url });
    } else {
      await invoke("open_path", { path: loc.path });
    }
    openLocationMsgEl.textContent = "";
  } catch (err) {
    openLocationMsgEl.textContent = `couldn't open cloud provider location: ${err}`;
  }
}

// Shown only when the currently loaded versions include a pending
// (offline, unsynced) one — see loadVersions().
async function openBackupsLocation() {
  if (selectedAppId === null) return;
  openLocationMsgEl.textContent = "looking for backups location…";
  try {
    const loc = await invoke("get_pending_location", { appId: selectedAppId });
    await invoke("open_path", { path: loc.path });
    openLocationMsgEl.textContent = "";
  } catch (err) {
    openLocationMsgEl.textContent = `couldn't open backups location: ${err}`;
  }
}

// ---- versions for the selected game ------------------------------------

// Bumped per call so a slow reply for a previously selected game can't
// overwrite the list of the one now selected.
let versionsRequest = 0;

// Switching games (`fresh`) swaps the list for skeleton rows; refreshes
// after restore/pin/delete keep the current rows and only show the spinner.
async function loadVersions(fresh = false) {
  if (selectedAppId === null) return;
  const request = ++versionsRequest;
  versionsSpinnerEl.classList.remove("hidden");
  if (fresh) {
    openBackupsBtnEl.classList.add("hidden");
    versionsListEl.innerHTML = "";
    versionsListEl.setAttribute("aria-busy", "true");
    for (let i = 0; i < 3; i++) {
      const row = document.createElement("div");
      row.className = "version-row skeleton";
      row.innerHTML = '<div class="skeleton-lines"><span></span><span></span></div>';
      versionsListEl.appendChild(row);
    }
  }
  try {
    const versions = await invoke("list_versions", { appId: selectedAppId });
    if (request !== versionsRequest) return;
    versionsListEl.innerHTML = "";
    if (versions.length === 0) {
      versionsListEl.innerHTML = '<p class="muted">no versions yet</p>';
    } else {
      versions.sort((a, b) => b.created - a.created);
      for (const v of versions) {
        versionsListEl.appendChild(renderVersionRow(v));
      }
    }
    openBackupsBtnEl.classList.toggle("hidden", !versions.some((v) => v.pending));
  } catch (err) {
    if (request !== versionsRequest) return;
    versionsListEl.innerHTML = "";
    const msg = document.createElement("p");
    msg.className = "muted";
    msg.textContent = `couldn't load versions: ${err}`;
    versionsListEl.appendChild(msg);
  } finally {
    if (request === versionsRequest) {
      versionsSpinnerEl.classList.add("hidden");
      versionsListEl.removeAttribute("aria-busy");
    }
  }
}

function renderVersionRow(v) {
  const row = document.createElement("div");
  row.className = "version-row";

  const info = document.createElement("div");
  info.className = "version-info";
  const badges = [
    v.active && '<span class="badge active">active</span>',
    v.pinned && '<span class="badge pinned">pinned</span>',
    v.pending && '<span class="badge pending" title="Saved locally — not yet synced to the cloud">pending — offline</span>',
  ]
    .filter(Boolean)
    .join(" ");
  info.innerHTML = `
    <div>${humanDate(v.created)} ${badges}</div>
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

async function browseLocalFolder() {
  try {
    const path = await invoke("pick_local_folder");
    if (path) {
      providerLocalPathEl.value = path;
    }
  } catch (err) {
    setStatus(`folder picker failed: ${err}`);
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

// ---- auto-update (D20) ----------------------------------------------------

// Per-viewer preference like the theme; defaults to on when unset or when
// storage is unavailable.
function autoUpdateEnabled() {
  try {
    return localStorage.getItem(AUTO_UPDATE_KEY) !== "off";
  } catch {
    return true;
  }
}

async function loadUpdateSettings() {
  updateAutoToggleEl.checked = autoUpdateEnabled();
  try {
    updateCurrentEl.textContent = `Current version: ${await invoke("app_version")}`;
  } catch {
    updateCurrentEl.textContent = "";
  }
}

// `manual` = user clicked "Check now": report "up to date" and errors.
// Background checks stay silent unless an update is actually found.
async function checkForUpdate(manual) {
  if (manual) {
    updateMsgEl.textContent = "checking…";
    updateCheckBtnEl.disabled = true;
  }
  try {
    const update = await invoke("check_update");
    if (update) {
      updateBannerTextEl.textContent = `Update available: YASGM ${update.version} (you have ${update.current})`;
      updateBannerTextEl.title = update.notes || "";
      updateBannerEl.classList.remove("hidden");
      if (manual) updateMsgEl.textContent = `version ${update.version} is available`;
    } else {
      updateBannerEl.classList.add("hidden");
      if (manual) updateMsgEl.textContent = "you're up to date";
    }
  } catch (err) {
    if (manual) updateMsgEl.textContent = `update check failed: ${err}`;
    else console.warn("background update check failed:", err);
  } finally {
    updateCheckBtnEl.disabled = false;
  }
}

async function installUpdate() {
  updateInstallBtnEl.disabled = true;
  updateBannerTextEl.textContent = "Stopping sync daemon, downloading and installing… the app will restart.";
  try {
    // Only returns on failure: success relaunches the app.
    await invoke("install_update");
  } catch (err) {
    updateBannerTextEl.textContent = `${err}`;
    updateInstallBtnEl.disabled = false;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  gamesListEl = document.querySelector("#games-list");
  detailEmptyEl = document.querySelector("#detail-empty");
  detailContentEl = document.querySelector("#detail-content");
  detailTitleEl = document.querySelector("#detail-title");
  detailAppIdEl = document.querySelector("#detail-appid");
  detailWarningEl = document.querySelector("#detail-warning");
  modeSelectEl = document.querySelector("#mode-select");
  keepInputEl = document.querySelector("#keep-input");
  saveBtnEl = document.querySelector("#save-btn");
  resetBtnEl = document.querySelector("#reset-btn");
  openSavesBtnEl = document.querySelector("#open-saves-btn");
  openProviderBtnEl = document.querySelector("#open-provider-btn");
  openBackupsBtnEl = document.querySelector("#open-backups-btn");
  openLocationMsgEl = document.querySelector("#open-location-msg");
  versionsListEl = document.querySelector("#versions-list");
  versionsSpinnerEl = document.querySelector("#versions-spinner");
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
  providerLocalBrowseBtnEl = document.querySelector("#provider-local-browse-btn");
  providerLocalBtnEl = document.querySelector("#provider-local-btn");
  autostartToggleEl = document.querySelector("#autostart-toggle");
  autostartDetailEl = document.querySelector("#autostart-detail");
  updateBannerEl = document.querySelector("#update-banner");
  updateBannerTextEl = document.querySelector("#update-banner-text");
  updateInstallBtnEl = document.querySelector("#update-install-btn");
  updateCurrentEl = document.querySelector("#update-current");
  updateAutoToggleEl = document.querySelector("#update-auto-toggle");
  updateCheckBtnEl = document.querySelector("#update-check-btn");
  updateMsgEl = document.querySelector("#update-msg");

  saveBtnEl.addEventListener("click", saveSelectedGame);
  resetBtnEl.addEventListener("click", resetSelectedGame);
  openSavesBtnEl.addEventListener("click", openSaveLocation);
  openProviderBtnEl.addEventListener("click", openProviderLocation);
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
  providerLocalBrowseBtnEl.addEventListener("click", browseLocalFolder);
  providerLocalBtnEl.addEventListener("click", useLocalFolder);
  autostartToggleEl.addEventListener("change", toggleAutostart);
  updateInstallBtnEl.addEventListener("click", installUpdate);
  updateCheckBtnEl.addEventListener("click", () => checkForUpdate(true));
  updateAutoToggleEl.addEventListener("change", () => {
    try {
      localStorage.setItem(AUTO_UPDATE_KEY, updateAutoToggleEl.checked ? "on" : "off");
    } catch {}
  });

  loadTheme();
  loadAccentColor();
  loadGames().then(loadIcons);
  loadUpdateSettings();
  if (autoUpdateEnabled()) checkForUpdate(false);
  setInterval(() => {
    if (autoUpdateEnabled()) checkForUpdate(false);
  }, UPDATE_CHECK_INTERVAL_MS);
});
