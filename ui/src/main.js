const { invoke } = window.__TAURI__.core;

let appIdInputEl;
let versionsBodyEl;
let statusMsgEl;

function humanBytes(bytes) {
  if (bytes >= 1_073_741_824) return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

async function loadVersions() {
  statusMsgEl.textContent = "loading…";
  versionsBodyEl.innerHTML = "";
  const raw = appIdInputEl.value.trim();
  const appId = raw === "" ? null : Number(raw);
  try {
    const versions = await invoke("list_versions", { appId });
    if (versions.length === 0) {
      statusMsgEl.textContent = "no versions found";
      return;
    }
    for (const v of versions) {
      versionsBodyEl.appendChild(renderRow(v));
    }
    statusMsgEl.textContent = `${versions.length} version(s)`;
  } catch (err) {
    statusMsgEl.textContent = `error: ${err}`;
  }
}

function renderRow(v) {
  const tr = document.createElement("tr");

  const label = v.active ? " [active]" : v.pinned ? " [pinned]" : "";
  const cells = [v.game, v.id + label, v.machine, v.os, v.files, humanBytes(v.size)];
  for (const text of cells) {
    const td = document.createElement("td");
    td.textContent = text;
    tr.appendChild(td);
  }

  const actionTd = document.createElement("td");
  const restoreBtn = document.createElement("button");
  restoreBtn.textContent = "Restore";
  restoreBtn.disabled = v.active;
  restoreBtn.addEventListener("click", () => restoreVersion(v));
  actionTd.appendChild(restoreBtn);
  tr.appendChild(actionTd);

  return tr;
}

async function restoreVersion(v) {
  statusMsgEl.textContent = `restoring ${v.id}…`;
  try {
    const result = await invoke("restore_version", { appId: v.app_id, versionId: v.id });
    statusMsgEl.textContent = result.trim() || "restored";
    await loadVersions();
  } catch (err) {
    statusMsgEl.textContent = `restore failed: ${err}`;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  appIdInputEl = document.querySelector("#app-id-input");
  versionsBodyEl = document.querySelector("#versions-body");
  statusMsgEl = document.querySelector("#status-msg");
  document.querySelector("#load-form").addEventListener("submit", (e) => {
    e.preventDefault();
    loadVersions();
  });
  loadVersions();
});
