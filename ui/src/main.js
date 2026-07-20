const { invoke } = window.__TAURI__.core;

let appIdInputEl;
let versionsBodyEl;
let gamesBodyEl;
let statusMsgEl;

const MODES = ["auto", "sync", "backup", "off"];

function humanBytes(bytes) {
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

function setStatus(text) {
  statusMsgEl.textContent = text;
}

// ---- games / per-game config ----------------------------------------------

async function loadGames() {
  gamesBodyEl.innerHTML = "";
  try {
    const games = await invoke("list_games");
    for (const g of games) {
      gamesBodyEl.appendChild(renderGameRow(g));
    }
  } catch (err) {
    setStatus(`error loading games: ${err}`);
  }
}

function renderGameRow(g) {
  const tr = document.createElement("tr");

  const nameTd = document.createElement("td");
  nameTd.textContent = g.game;
  tr.appendChild(nameTd);

  const idTd = document.createElement("td");
  idTd.textContent = g.app_id;
  tr.appendChild(idTd);

  const modeTd = document.createElement("td");
  const modeSelect = document.createElement("select");
  for (const m of MODES) {
    const opt = document.createElement("option");
    opt.value = m;
    opt.textContent = m;
    if (m === g.mode) opt.selected = true;
    modeSelect.appendChild(opt);
  }
  modeTd.appendChild(modeSelect);
  tr.appendChild(modeTd);

  const keepTd = document.createElement("td");
  const keepInput = document.createElement("input");
  keepInput.type = "number";
  keepInput.min = "1";
  keepInput.placeholder = `default (${g.default_keep})`;
  keepInput.className = "keep-input";
  if (g.keep !== null && g.keep !== undefined) keepInput.value = g.keep;
  keepTd.appendChild(keepInput);
  tr.appendChild(keepTd);

  const actionTd = document.createElement("td");
  const saveBtn = document.createElement("button");
  saveBtn.textContent = "Save";
  saveBtn.addEventListener("click", async () => {
    setStatus(`saving ${g.game}…`);
    try {
      const keep = keepInput.value === "" ? null : Number(keepInput.value);
      const result = await invoke("set_game_config", {
        appId: g.app_id,
        mode: modeSelect.value,
        keep,
      });
      setStatus(result.trim() || "saved");
      await loadGames();
    } catch (err) {
      setStatus(`save failed: ${err}`);
    }
  });
  actionTd.appendChild(saveBtn);

  const resetBtn = document.createElement("button");
  resetBtn.textContent = "Reset";
  resetBtn.title = "Clear overrides (back to auto mode, default keep count)";
  resetBtn.addEventListener("click", async () => {
    setStatus(`resetting ${g.game}…`);
    try {
      const result = await invoke("clear_game_config", { appId: g.app_id });
      setStatus(result.trim() || "reset");
      await loadGames();
    } catch (err) {
      setStatus(`reset failed: ${err}`);
    }
  });
  actionTd.appendChild(resetBtn);

  tr.appendChild(actionTd);
  return tr;
}

// ---- versions / restore / pin / delete -------------------------------------

async function loadVersions() {
  setStatus("loading…");
  versionsBodyEl.innerHTML = "";
  const raw = appIdInputEl.value.trim();
  const appId = raw === "" ? null : Number(raw);
  try {
    const versions = await invoke("list_versions", { appId });
    if (versions.length === 0) {
      setStatus("no versions found");
      return;
    }
    for (const v of versions) {
      versionsBodyEl.appendChild(renderVersionRow(v));
    }
    setStatus(`${versions.length} version(s)`);
  } catch (err) {
    setStatus(`error: ${err}`);
  }
}

function renderVersionRow(v) {
  const tr = document.createElement("tr");

  const label = v.active ? " [active]" : v.pinned ? " [pinned]" : "";
  const cells = [v.game, v.id + label, v.machine, v.os, v.files, humanBytes(v.size)];
  for (const text of cells) {
    const td = document.createElement("td");
    td.textContent = text;
    tr.appendChild(td);
  }

  const restoreTd = document.createElement("td");
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
  restoreTd.appendChild(restoreBtn);
  tr.appendChild(restoreTd);

  const pinTd = document.createElement("td");
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
  pinTd.appendChild(pinBtn);
  tr.appendChild(pinTd);

  const deleteTd = document.createElement("td");
  const deleteBtn = document.createElement("button");
  deleteBtn.textContent = "Delete";
  // Active head and pinned safety/conflict versions aren't meant to be
  // casually deleted from here (D5/D14) — pin state already gates that via
  // `yasgm rm`'s own no-op-on-missing behavior, but disabling avoids
  // surprise data loss from a stray click on the row you're restoring from.
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
  deleteTd.appendChild(deleteBtn);
  tr.appendChild(deleteTd);

  return tr;
}

window.addEventListener("DOMContentLoaded", () => {
  appIdInputEl = document.querySelector("#app-id-input");
  versionsBodyEl = document.querySelector("#versions-body");
  gamesBodyEl = document.querySelector("#games-body");
  statusMsgEl = document.querySelector("#status-msg");
  document.querySelector("#load-form").addEventListener("submit", (e) => {
    e.preventDefault();
    loadVersions();
  });
  loadGames();
  loadVersions();
});
