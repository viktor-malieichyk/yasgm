const { invoke } = window.__TAURI__.core;

const MODES = ["auto", "sync", "backup", "off"];
const THEME_KEY = "yasgm-theme";
const NO_CLOUD_KEY = "yasgm-only-no-steam-cloud";
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
let navSettingsEl;
let libraryViewEl;
let settingsViewEl;
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
// Sidebar filter: hide games Steam Cloud already syncs.
let onlyNoCloud = false;
// Sidebar search text, lowercased.
let searchQuery = "";

const CLOUD_ICON =
  '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M6 15.5a3.8 3.8 0 0 1-.4-7.58A5 5 0 0 1 15 7a3.6 3.6 0 0 1 .3 8.5H6z" /></svg>';

function visibleGames() {
  return games.filter(
    (g) => !(onlyNoCloud && g.steam_cloud) && (!searchQuery || g.game.toLowerCase().includes(searchQuery)),
  );
}
// appid → data: URL of the game's icon (see loadIcons).
let icons = {};

// ---- top-level view switching (Library / Settings) -----------------------

function showView(view) {
  const isLibrary = view === "library";
  libraryViewEl.classList.toggle("hidden", !isLibrary);
  settingsViewEl.classList.toggle("hidden", isLibrary);
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
  const shown = visibleGames();
  if (selectedAppId === null && shown.length > 0) {
    selectGame(shown[0].app_id);
  } else if (selectedAppId !== null && shown.some((g) => g.app_id === selectedAppId)) {
    renderDetailSettings();
  } else if (shown.length > 0) {
    selectGame(shown[0].app_id);
  } else {
    selectedAppId = null;
    showEmptyDetail();
  }
}

function setOnlyNoCloud(value) {
  onlyNoCloud = value;
  try {
    localStorage.setItem(NO_CLOUD_KEY, value ? "1" : "0");
  } catch {}
  loadGames();
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
  let why = m[2] === "override" ? "set manually" : (m[2] || "").replace(/^auto:?\s*/, "");
  // The cloud badge on the row already says it.
  if (g.steam_cloud && why === "Steam Cloud") why = "";
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
  if (visibleGames().length === 0 && games.length > 0) {
    const empty = document.createElement("p");
    empty.className = "muted list-empty";
    empty.textContent = searchQuery ? "No games match your search." : "Every installed game uses Steam Cloud.";
    gamesListEl.appendChild(empty);
  }
  for (const g of visibleGames()) {
    const item = document.createElement("button");
    item.type = "button";
    item.className =
      "game-item" +
      (g.app_id === selectedAppId ? " selected" : "") +
      (g.in_manifest ? "" : " unsupported") +
      (g.local_saves === false ? " no-saves" : "");
    item.title = g.in_manifest ? g.game : `${g.game} — no save locations known for this game`;
    const text = document.createElement("span");
    text.className = "game-text";
    const name = document.createElement("span");
    name.className = "game-name";
    name.textContent = g.game;
    const sub = document.createElement("span");
    sub.className = "game-sub";
    // Known location but nothing there yet (never played here, or saves
    // not restored yet).
    sub.textContent = g.local_saves === false ? `No saves here · ${shortMode(g)}` : shortMode(g);
    text.append(name, sub);
    item.append(gameIcon(g), text);
    if (g.steam_cloud) {
      const cloud = document.createElement("span");
      cloud.className = "cloud-badge";
      cloud.title = "Steam Cloud already syncs this game";
      cloud.innerHTML = CLOUD_ICON;
      item.append(cloud);
    }
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
  detailAppIdEl.textContent =
    `AppID ${g.app_id} · ${g.effective_mode}` + (g.local_saves === false ? " · no saves on this computer yet" : "");
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

// ---- settings: cloud sync ---------------------------------------------------

const cloudEls = {};
let signingIn = false;

// One provider at a time, and connecting one *is* choosing it, so there's no
// provider switch: this shows what you're connected to (with Disconnect), or
// the ways to connect.
async function loadProvider() {
  cloudEls.loading.classList.remove("hidden");
  cloudEls.connected.classList.add("hidden");
  cloudEls.disconnected.classList.add("hidden");
  let s;
  try {
    s = await invoke("get_cloud_status");
  } catch (err) {
    cloudEls.loading.textContent = `couldn't read the cloud settings: ${err}`;
    return;
  }
  cloudEls.loading.classList.add("hidden");
  cloudEls.confirm.classList.add("hidden");
  cloudEls.disconnectBtn.classList.remove("hidden");
  cloudEls.msg.textContent = "";

  const connected = s.provider.type === "local" || (s.provider.type === "onedrive" && s.signed_in);
  if (connected) {
    cloudEls.connected.classList.remove("hidden");
    cloudEls.title.textContent = s.provider.type === "local" ? `Folder: ${s.provider.path}` : "OneDrive";
    cloudEls.detail.textContent = s.ok ? s.detail : `Not working right now: ${s.error}`;
    return;
  }
  cloudEls.disconnected.classList.remove("hidden");
  cloudEls.note.textContent =
    s.provider.type === "onedrive"
      ? "You're signed out of OneDrive. Sign in again to keep syncing."
      : "Not connected — your saves aren't being backed up. Connect a cloud:";
  const busy = signingIn;
  cloudEls.qrPanel.classList.toggle("hidden", !busy);
  cloudEls.onedriveButtons.classList.toggle("hidden", busy);
}

async function checkProviderStatus() {
  cloudEls.msg.textContent = "checking…";
  try {
    const s = await invoke("get_cloud_status");
    cloudEls.msg.textContent = s.ok ? "connected" : `not working: ${s.error}`;
    cloudEls.detail.textContent = s.ok ? s.detail : `Not working right now: ${s.error}`;
  } catch (err) {
    cloudEls.msg.textContent = `check failed: ${err}`;
  }
}

async function disconnectProvider() {
  cloudEls.msg.textContent = "disconnecting…";
  try {
    const result = await invoke("disconnect_provider");
    setStatus(result.trim());
  } catch (err) {
    setStatus(`disconnect failed: ${err}`);
  }
  await loadProvider();
}

async function signInWithBrowser() {
  cloudEls.signinMsg.textContent = "opening your browser…";
  cloudEls.signinBrowserBtn.disabled = true;
  cloudEls.signinQrBtn.disabled = true;
  try {
    await invoke("cloud_auth");
    cloudEls.signinMsg.textContent = "";
  } catch (err) {
    cloudEls.signinMsg.textContent = `sign-in failed: ${err}`;
  } finally {
    cloudEls.signinBrowserBtn.disabled = false;
    cloudEls.signinQrBtn.disabled = false;
  }
  await loadProvider();
}

// Device-code sign-in: the CLI streams JSON events (`auth-event`); the page
// shows a QR code of the Microsoft link plus the code to type on a phone.
async function signInWithQr() {
  signingIn = true;
  cloudEls.signinMsg.textContent = "";
  cloudEls.onedriveButtons.classList.add("hidden");
  cloudEls.qrPanel.classList.remove("hidden");
  cloudEls.qrImg.removeAttribute("src");
  cloudEls.qrLink.textContent = "";
  cloudEls.qrCode.textContent = "…";
  cloudEls.qrStatus.textContent = "getting a sign-in code…";
  try {
    await invoke("start_device_sign_in");
  } catch (err) {
    endQrSignIn(`couldn't start sign-in: ${err}`);
  }
}

function endQrSignIn(message) {
  signingIn = false;
  cloudEls.qrPanel.classList.add("hidden");
  cloudEls.onedriveButtons.classList.remove("hidden");
  cloudEls.signinMsg.textContent = message || "";
}

async function cancelQrSignIn() {
  await invoke("cancel_device_sign_in").catch(() => {});
  endQrSignIn();
}

function onAuthEvent(event) {
  if (!signingIn) return;
  switch (event.event) {
    case "code": {
      const qr = qrcode(0, "M");
      qr.addData(event.verification_uri);
      qr.make();
      cloudEls.qrImg.src = qr.createDataURL(5, 2);
      cloudEls.qrLink.textContent = event.verification_uri.replace(/^https?:\/\//, "");
      cloudEls.qrCode.textContent = event.user_code;
      cloudEls.qrStatus.textContent = `waiting for you to sign in… (code valid for ${Math.round(event.expires_in / 60)} minutes)`;
      break;
    }
    case "signed_in":
      endQrSignIn();
      setStatus("signed in to OneDrive");
      loadProvider();
      break;
    case "error":
      endQrSignIn(`sign-in failed: ${event.message}`);
      break;
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
  setStatus(`connecting the folder ${path}…`);
  try {
    const result = await invoke("set_provider_local", { path });
    setStatus(result.trim() || "folder connected");
    await loadProvider();
  } catch (err) {
    setStatus(`couldn't use that folder: ${err}`);
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
  navSettingsEl = document.querySelector("#nav-settings");
  libraryViewEl = document.querySelector("#library-view");
  settingsViewEl = document.querySelector("#settings-view");
  Object.assign(cloudEls, {
    loading: document.querySelector("#cloud-loading"),
    connected: document.querySelector("#cloud-connected"),
    disconnected: document.querySelector("#cloud-disconnected"),
    title: document.querySelector("#cloud-connected-title"),
    detail: document.querySelector("#cloud-connected-detail"),
    msg: document.querySelector("#cloud-msg"),
    confirm: document.querySelector("#cloud-confirm"),
    disconnectBtn: document.querySelector("#cloud-disconnect-btn"),
    note: document.querySelector("#cloud-disconnected-note"),
    onedriveButtons: document.querySelector("#onedrive-buttons"),
    signinQrBtn: document.querySelector("#signin-qr-btn"),
    signinBrowserBtn: document.querySelector("#signin-browser-btn"),
    signinMsg: document.querySelector("#signin-msg"),
    qrPanel: document.querySelector("#qr-panel"),
    qrImg: document.querySelector("#qr-img"),
    qrLink: document.querySelector("#qr-link"),
    qrCode: document.querySelector("#qr-code"),
    qrStatus: document.querySelector("#qr-status"),
  });
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

  // Settings is a toggle; picking a game (or clicking Settings again) goes
  // back to the library, so there's no separate Library button.
  navSettingsEl.addEventListener("click", () =>
    showView(settingsViewEl.classList.contains("hidden") ? "settings" : "library"),
  );

  for (const seg of themeSwitchEl.querySelectorAll(".segment")) {
    seg.addEventListener("click", () => {
      const theme = seg.dataset.themeOption;
      localStorage.setItem(THEME_KEY, theme);
      applyTheme(theme);
    });
  }

  document.querySelector("#cloud-check-btn").addEventListener("click", checkProviderStatus);
  cloudEls.disconnectBtn.addEventListener("click", () => {
    cloudEls.disconnectBtn.classList.add("hidden");
    cloudEls.confirm.classList.remove("hidden");
  });
  document.querySelector("#cloud-disconnect-no").addEventListener("click", () => {
    cloudEls.confirm.classList.add("hidden");
    cloudEls.disconnectBtn.classList.remove("hidden");
  });
  document.querySelector("#cloud-disconnect-yes").addEventListener("click", disconnectProvider);
  cloudEls.signinQrBtn.addEventListener("click", signInWithQr);
  cloudEls.signinBrowserBtn.addEventListener("click", signInWithBrowser);
  document.querySelector("#qr-cancel-btn").addEventListener("click", cancelQrSignIn);
  window.__TAURI__.event.listen("auth-event", (e) => onAuthEvent(e.payload));
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

  try {
    onlyNoCloud = localStorage.getItem(NO_CLOUD_KEY) === "1";
  } catch {}
  const noCloudToggle = document.querySelector("#filter-no-cloud");
  noCloudToggle.checked = onlyNoCloud;
  noCloudToggle.addEventListener("change", () => setOnlyNoCloud(noCloudToggle.checked));
  // Filters the list only; the selected game stays open.
  const searchEl = document.querySelector("#game-search");
  searchEl.addEventListener("input", () => {
    searchQuery = searchEl.value.trim().toLowerCase();
    renderGamesList();
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
