import {
  ButtonItem,
  ConfirmModal,
  DialogButton,
  DropdownItem,
  Field,
  Focusable,
  Navigation,
  PanelSection,
  PanelSectionRow,
  ProgressBarWithInfo,
  Spinner,
  TextField,
  ToggleField,
  showModal,
  staticClasses,
  useParams,
} from "@decky/ui";
import { addEventListener, callable, definePlugin, removeEventListener, routerHook, toaster } from "@decky/api";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FaCloud, FaCloudUploadAlt } from "react-icons/fa";
import qrcode from "qrcode-generator";

// ---- backend (main.py) ------------------------------------------------------

/** `yasgm status --json`, plus the daemon's `watch send status` reply. */
interface Status {
  provider: { type: "onedrive" | "local" | "none"; path?: string };
  signed_in: boolean | null;
  ok: boolean;
  detail: string | null;
  error: string | null;
  daemon: { running: boolean; pid: number | null };
}

/** The daemon's live state over its socket (`get_daemon_state`, polled). */
interface DaemonState {
  running: boolean;
  /** Not reachable, but the plugin has started it (coming up). */
  starting?: boolean;
  error?: string;
  paused?: boolean;
  syncing?: boolean;
  /** The game being synced; null while syncing every game. */
  syncing_game?: number | null;
  /** What the sync is doing: "checking" | "uploading" | "downloading". */
  sync_phase?: string | null;
  running_game?: number | null;
  pending_sync?: number[];
  watching_games?: number;
}

/** A plugin release on GitHub (`check_update`). */
interface Release {
  version: string;
  url: string;
  hash: string;
  notes: string;
}

interface UpdateInfo {
  current: string;
  update: Release | null;
  previous: Release | null;
  checked_at: number;
}

/** One row of `yasgm games --json`. */
interface Game {
  app_id: number;
  game: string;
  mode: string;
  effective_mode: string;
  in_manifest: boolean;
  steam_cloud: boolean;
  /** Save files exist on this Deck; null when the location isn't known. */
  local_saves: boolean | null;
  /** Saves in the cloud, game not installed on this Deck (from `get_cloud_games`). */
  cloud_only?: boolean;
  latest_machine?: string | null;
  cross_os_note: string | null;
  last_version: string | null;
  synced_at: number | null;
  keep: number | null;
  default_keep: number;
}

/** One row of `yasgm cloud-games --json`. */
interface CloudGame {
  app_id: number;
  game: string;
  versions: number;
  latest_at: number | null;
  latest_machine: string | null;
  latest_os: string | null;
}

/** A cloud-only game as a list row: its versions can be browsed, nothing else. */
const cloudRow = (c: CloudGame): Game => ({
  app_id: c.app_id,
  game: c.game,
  mode: "off",
  effective_mode: "Not installed on this Deck",
  in_manifest: true,
  steam_cloud: false,
  local_saves: false,
  cloud_only: true,
  latest_machine: c.latest_machine,
  cross_os_note: null,
  last_version: null,
  synced_at: null,
  keep: null,
  default_keep: 10,
});

/** One entry of the daemon's event log (`watch send events`). */
interface SyncEvent {
  seq: number;
  at: number;
  app_id: number | null;
  game: string | null;
  kind: "uploaded" | "downloaded" | "conflict" | "cloud_ahead" | "quota_exceeded" | "error";
  detail: string;
}

/** One row of `yasgm versions <appid> --json`. */
interface Version {
  id: string;
  app_id: number;
  created: number;
  machine: string;
  os: string;
  files: number;
  size: number;
  active: boolean;
  pinned: boolean;
  pending: boolean;
}

type AuthEvent =
  | { event: "code"; user_code: string; verification_uri: string; expires_in: number }
  | { event: "waiting"; elapsed: number }
  | { event: "signed_in"; detail: string }
  | { event: "not_needed"; detail: string }
  | { event: "error"; message: string };

// `fresh: false` answers from the backend's cache (instant once warm).
const getStatus = callable<[fresh: boolean], Status>("get_status");
const getDaemonState = callable<[], DaemonState>("get_daemon_state");
const getGames = callable<[fresh: boolean], Game[]>("get_games");
const getCloudGames = callable<[fresh: boolean], CloudGame[]>("get_cloud_games");
const getIcons = callable<[fresh: boolean], Record<string, string>>("get_icons");
const checkUpdate = callable<[force: boolean], UpdateInfo>("check_update");
const getVersions = callable<[appId: number], Version[]>("get_versions");
const syncNow = callable<[appId: number], string>("sync_now");
const setPaused = callable<[paused: boolean], string>("set_paused");
const gameEvent = callable<[appId: number, running: boolean, pid: number], void>("game_event");
const setKeep = callable<[appId: number, keep: number], string>("set_keep");
const restoreVersion = callable<[appId: number, versionId: string], string>("restore");
const setPinned = callable<[appId: number, versionId: string, pinned: boolean], string>("set_pinned");
const setMode = callable<[appId: number, mode: string], string>("set_mode");
const disconnectCloud = callable<[], string>("disconnect");
const startSignIn = callable<[], void>("start_sign_in");
const cancelSignIn = callable<[], void>("cancel_sign_in");

const KEEP_CHOICES = [3, 5, 10, 20, 50];

// Decky Loader's own installer (its `utilities/install_plugin` route): it
// asks the user to confirm, downloads the zip, checks the SHA-256, replaces
// this plugin and reloads it — the plugin itself can't write its folder.
declare global {
  interface Window {
    DeckyBackend: { call<Args extends unknown[], R = void>(route: string, ...args: Args): Promise<R> };
  }
}
const PLUGIN_NAME = "YASGM"; // plugin.json "name": the install Decky replaces
const INSTALL_UPDATE = 2;
const INSTALL_DOWNGRADE = 3;

function installRelease(release: Release, type: number) {
  return window.DeckyBackend.call("utilities/install_plugin", release.url, PLUGIN_NAME, release.version, release.hash, type);
}

/** Toast text for a sync event from the daemon. */
function eventToast(e: SyncEvent): string {
  const game = e.game ?? "a game";
  switch (e.kind) {
    case "uploaded":
      return e.detail === "offline" ? `Saves kept on this Deck until you're online — ${game}` : `Saves backed up — ${game}`;
    case "downloaded":
      return `Newer saves from ${e.detail} restored — ${game}`;
    case "conflict":
      return `${game}: saves changed here and on another device — kept both; the other version is pinned`;
    case "cloud_ahead":
      return `${game}: newer saves from ${e.detail} are in the cloud — restore them in YASGM if you want them`;
    case "quota_exceeded":
      return "OneDrive is full — syncing stopped until there's space";
    case "error":
      return `Sync failed: ${e.detail.length > 140 ? e.detail.slice(0, 140) + "…" : e.detail}`;
  }
}

/** Toasts for one batch of daemon events; a burst of uploads or downloads
 *  (the daemon's first pass, Sync now) becomes a single toast. */
function toastEvents(events: SyncEvent[]) {
  const bursts: Record<string, SyncEvent[]> = {};
  const single: SyncEvent[] = [];
  for (const e of events) {
    if ((e.kind === "uploaded" && e.detail !== "offline") || e.kind === "downloaded") (bursts[e.kind] ??= []).push(e);
    else single.push(e);
  }
  for (const [kind, list] of Object.entries(bursts)) {
    if (list.length > 2) {
      const what = kind === "uploaded" ? "Saves backed up" : "Newer saves restored";
      toaster.toast({ title: "YASGM", body: `${what} — ${list.length} games` });
    } else single.push(...list);
  }
  for (const e of single) toaster.toast({ title: "YASGM", body: eventToast(e) });
}

const MODES = [
  { data: "auto", label: "Auto" },
  { data: "sync", label: "Sync" },
  { data: "backup", label: "Backup only" },
  { data: "off", label: "Off" },
];

// ---- helpers ----------------------------------------------------------------

const errorText = (err: unknown) => (err instanceof Error ? err.message : String(err));

function humanBytes(bytes: number) {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(1)} GB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

function timeAgo(unixSeconds: number) {
  const secs = Math.max(0, Date.now() / 1000 - unixSeconds);
  if (secs < 90) return "just now";
  if (secs < 3600) return `${Math.round(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)} h ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString();
}

/** "backup-only (auto: Steam Cloud)" → "Backup only · Steam Cloud" (as the desktop GUI does). */
function shortMode(g: Game) {
  if (!g.in_manifest) return "No known save location";
  const m = g.effective_mode.match(/^([\w-]+)(?: \((.*)\))?$/);
  if (!m) return g.effective_mode;
  const mode = m[1].replace("-", " ");
  const label = mode[0].toUpperCase() + mode.slice(1);
  const why = m[2] === "override" ? "set manually" : (m[2] || "").replace(/^auto:?\s*/, "");
  return why ? `${label} · ${why}` : label;
}

const muted = { fontSize: "12px", color: "#8b929a" };

const NO_CLOUD_KEY = "yasgm-only-no-steam-cloud";

const LOCAL_ONLY_KEY = "yasgm-only-local-saves";

function loadOnlyNoCloud() {
  try {
    return localStorage.getItem(NO_CLOUD_KEY) === "1";
  } catch {
    return false;
  }
}

/** "Only with saves here" — on unless turned off. */
function loadOnlyLocal() {
  try {
    return localStorage.getItem(LOCAL_ONLY_KEY) !== "0";
  } catch {
    return true;
  }
}

/** Installed games and cloud-only ones: the backend's cached lists at once,
 *  then fresh ones. `reload(true)` re-reads after an action. */
function useGameLists() {
  const [games, setGames] = useState<Game[] | null>(null);
  const [cloud, setCloud] = useState<Game[]>([]);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async (fresh: boolean) => {
    const loadGames = getGames(fresh)
      .then((g) => {
        setGames(g);
        setError(null);
      })
      .catch((err) => setError(errorText(err)));
    const loadCloud = getCloudGames(fresh)
      .then((c) => setCloud(c.map(cloudRow)))
      .catch(() => {});
    await Promise.all([loadGames, loadCloud]);
  }, []);
  useEffect(() => {
    reload(false).then(() => reload(true));
  }, []);
  return { games, cloud, error, reload };
}

const PHASE_TEXT: Record<string, string> = {
  waiting: "Waiting for another sync…",
  checking: "Checking saves…",
  uploading: "Uploading saves…",
  downloading: "Downloading newer saves…",
};
const phaseText = (d: DaemonState | null) => PHASE_TEXT[d?.sync_phase ?? ""] ?? "Syncing…";

// Icons rarely change: fetched once per session, shared by every view.
let iconCache: Record<string, string> = {};
let iconLoad: Promise<void> | null = null;

function useIcons() {
  const [icons, setIcons] = useState(iconCache);
  useEffect(() => {
    iconLoad ??= getIcons(false)
      .then((found) => {
        iconCache = found;
      })
      .catch(() => {
        iconLoad = null; // try again next time
      });
    iconLoad.then(() => setIcons(iconCache));
  }, []);
  return icons;
}

function GameIcon({ game, icons, size = 28 }: { game: Game; icons: Record<string, string>; size?: number }) {
  const url = icons[String(game.app_id)];
  // Greyed out when there are no saves on this Deck yet.
  const faded = game.local_saves === false ? { filter: "grayscale(1)", opacity: 0.45 } : {};
  const box = { width: size, height: size, borderRadius: 4, flexShrink: 0, ...faded } as const;
  if (url) return <img src={url} style={box} />;
  const letter = (game.game.match(/[\p{L}\p{N}]/u) ?? ["?"])[0].toUpperCase();
  return (
    <div
      style={{
        ...box,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        fontWeight: 700,
        fontSize: size * 0.5,
        background: `hsl(${(game.app_id * 47) % 360} 45% 35%)`,
      }}
    >
      {letter}
    </div>
  );
}

/** Polls the daemon's live state every 2 s while mounted; `onSyncDone` runs
 *  when a sync finishes (to re-read last-synced times and versions). */
function useDaemonState(onSyncDone?: () => void) {
  const [state, setState] = useState<DaemonState | null>(null);
  const done = useRef(onSyncDone);
  done.current = onSyncDone;
  useEffect(() => {
    let alive = true;
    let wasSyncing = false;
    const tick = async () => {
      try {
        const next = await getDaemonState();
        if (!alive) return;
        setState(next);
        if (wasSyncing && !next.syncing) done.current?.();
        wasSyncing = !!next.syncing;
      } catch {}
    };
    tick();
    const timer = setInterval(tick, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);
  return state;
}

const TONES = { ok: "#59bf40", busy: "#1a9fff", warn: "#e8b45f", bad: "#ff6b6b" };

/** The big "is it working?" card at the top of the panel. */
function StatusCard({ status, daemon, games }: { status: Status | null; daemon: DaemonState | null; games: Game[] | null }) {
  const name = (id: number | null | undefined) => games?.find((g) => g.app_id === id)?.game ?? "a game";
  const lastSync = Math.max(0, ...(games ?? []).map((g) => g.synced_at ?? 0));
  const notConnected = status && (status.provider.type === "none" || (status.provider.type === "onedrive" && !status.signed_in));

  let tone: keyof typeof TONES, title: string, detail: string;
  let spinning = false;
  if (!daemon) {
    [tone, title, detail, spinning] = ["busy", "Checking…", "", true];
  } else if (!daemon.running) {
    tone = "bad";
    title = "Sync is not running";
    detail = daemon.starting ? "Starting…" : "Restarting automatically. If this stays, see ~/homebrew/logs/yasgm/daemon.log";
  } else if (notConnected) {
    [tone, title, detail] = ["warn", "Not connected", "Your saves aren't backed up. Sign in below."];
  } else if (status && !status.ok) {
    [tone, title, detail] = ["warn", "Cloud unreachable", status.error ?? "Saves are kept here and uploaded later."];
  } else if (daemon.syncing) {
    tone = "busy";
    title = daemon.syncing_game ? `Syncing ${name(daemon.syncing_game)}…` : "Syncing all games…";
    detail = "Uploading or downloading saves";
    spinning = true;
  } else if (daemon.paused) {
    [tone, title, detail] = ["warn", "Paused", "Saves aren't synced until you resume."];
  } else if (daemon.running_game) {
    [tone, title, detail] = ["ok", `Playing ${name(daemon.running_game)}`, "Its saves sync when you quit."];
  } else {
    tone = "ok";
    title = "Sync is active";
    const pending = daemon.pending_sync?.length ?? 0;
    detail = pending
      ? `Waiting to sync ${pending} game${pending > 1 ? "s" : ""}`
      : `Watching ${daemon.watching_games ?? 0} games` + (lastSync ? ` · last sync ${timeAgo(lastSync)}` : "");
  }

  const color = TONES[tone];
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 12,
        padding: "10px 12px",
        margin: "4px 0 8px",
        borderRadius: 6,
        background: `${color}22`,
        borderLeft: `4px solid ${color}`,
      }}
    >
      {spinning ? (
        <Spinner style={{ width: 18, height: 18, flexShrink: 0 }} />
      ) : (
        <div style={{ width: 12, height: 12, borderRadius: 6, flexShrink: 0, background: color, boxShadow: `0 0 8px ${color}` }} />
      )}
      <div style={{ minWidth: 0 }}>
        <div style={{ fontWeight: 700, fontSize: "15px", color }}>{title}</div>
        {detail && <div style={{ ...muted, overflowWrap: "anywhere" }}>{detail}</div>}
      </div>
    </div>
  );
}

// ---- sign-in ------------------------------------------------------------------

/** OneDrive device-code sign-in: a QR code of the link and the code to enter
 *  on a phone, while `yasgm auth --device --json` waits in the backend. */
function SignIn({ onDone }: { onDone: () => void }) {
  const [event, setEvent] = useState<AuthEvent | null>(null);

  useEffect(() => {
    const listener = addEventListener<[AuthEvent]>("yasgm_auth", (e) => {
      if (e.event === "waiting") return;
      setEvent(e);
      if (e.event === "signed_in" || e.event === "not_needed") {
        toaster.toast({ title: "YASGM", body: "Signed in to OneDrive" });
        onDone();
      }
    });
    startSignIn().catch((err) => setEvent({ event: "error", message: errorText(err) }));
    return () => {
      removeEventListener("yasgm_auth", listener);
      cancelSignIn();
    };
  }, []);

  if (!event) {
    return (
      <PanelSectionRow>
        <Field label="Getting a sign-in code…">
          <Spinner style={{ width: 20 }} />
        </Field>
      </PanelSectionRow>
    );
  }
  if (event.event === "error") {
    return (
      <PanelSectionRow>
        <Field label="Sign-in failed" description={event.message} />
      </PanelSectionRow>
    );
  }
  if (event.event !== "code") return null;

  const qr = qrcode(0, "M");
  qr.addData(event.verification_uri);
  qr.make();
  return (
    <>
      <PanelSectionRow>
        <div style={{ display: "flex", justifyContent: "center", padding: "8px 0" }}>
          <img src={qr.createDataURL(4, 2)} style={{ width: 160, height: 160, imageRendering: "pixelated" }} />
        </div>
      </PanelSectionRow>
      <PanelSectionRow>
        <Field label="On your phone, open" description={event.verification_uri} />
      </PanelSectionRow>
      <PanelSectionRow>
        <Field label="and enter the code">
          <span style={{ fontSize: "20px", fontWeight: 700, letterSpacing: "2px" }}>{event.user_code}</span>
        </Field>
      </PanelSectionRow>
    </>
  );
}

// ---- quick access panel ------------------------------------------------------

function Content() {
  const [status, setStatus] = useState<Status | null>(null);
  const { games, cloud, error, reload } = useGameLists();
  const [busy, setBusy] = useState(false);
  const [signingIn, setSigningIn] = useState(false);
  const [query, setQuery] = useState("");
  const [onlyNoCloud, setOnlyNoCloud] = useState(loadOnlyNoCloud);
  const [onlyLocal, setOnlyLocal] = useState(loadOnlyLocal);
  const [updates, setUpdates] = useState<UpdateInfo | null>(null);
  const [updateNote, setUpdateNote] = useState<string | null>(null);
  const icons = useIcons();

  const daemon = useDaemonState(() => reload(true));

  // Everything loads independently: nothing waits for the slow parts.
  const refresh = useCallback(async (fresh = true) => {
    getStatus(fresh)
      .then(setStatus)
      .catch(() => {});
    await reload(fresh);
  }, []);

  useEffect(() => {
    getStatus(false)
      .then(setStatus)
      .then(() => getStatus(true).then(setStatus))
      .catch(() => {});
    checkUpdate(false)
      .then(setUpdates)
      .catch(() => {});
  }, []);

  const act = async (action: () => Promise<unknown>, done?: string) => {
    setBusy(true);
    try {
      await action();
      if (done) toaster.toast({ title: "YASGM", body: done });
    } catch (err) {
      toaster.toast({ title: "YASGM", body: errorText(err) });
    } finally {
      setBusy(false);
      refresh();
    }
  };

  const checkNow = async () => {
    setUpdateNote("Checking…");
    try {
      const info = await checkUpdate(true);
      setUpdates(info);
      setUpdateNote(info.update ? null : "You have the latest version");
    } catch (err) {
      setUpdateNote(`Couldn't check: ${errorText(err)}`);
    }
  };

  const toggleOnlyNoCloud = (value: boolean) => {
    setOnlyNoCloud(value);
    try {
      localStorage.setItem(NO_CLOUD_KEY, value ? "1" : "0");
    } catch {}
  };

  const toggleOnlyLocal = (value: boolean) => {
    setOnlyLocal(value);
    try {
      localStorage.setItem(LOCAL_ONLY_KEY, value ? "1" : "0");
    } catch {}
  };

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return [...(games ?? []), ...cloud]
      .filter(
        (g) =>
          !(onlyNoCloud && g.steam_cloud) &&
          !(onlyLocal && g.local_saves !== true) &&
          (!q || g.game.toLowerCase().includes(q)),
      )
      .sort((a, b) => a.game.localeCompare(b.game, undefined, { sensitivity: "base" }));
  }, [games, cloud, query, onlyNoCloud, onlyLocal]);

  const running = !!daemon?.running;
  const paused = daemon?.paused ?? false;
  // One provider at a time, and signing in is choosing it: no provider
  // switch. Disconnected (or signed out of OneDrive) → QR sign-in; connected
  // → Disconnect.
  const needsSignIn = status?.provider.type === "none" || (status?.provider.type === "onedrive" && !status.signed_in);
  const connected = !!status && !needsSignIn;
  const providerLabel = status?.provider.type === "local" ? `Folder: ${status.provider.path}` : "OneDrive";
  const update = updates?.update;

  return (
    <>
      <PanelSection title="Status">
        <PanelSectionRow>
          <StatusCard status={status} daemon={daemon} games={games} />
        </PanelSectionRow>
        {error && (
          <PanelSectionRow>
            <Field label="Something went wrong" description={error} />
          </PanelSectionRow>
        )}
        {update && (
          <PanelSectionRow>
            <ButtonItem layout="below" description={`You have ${updates?.current}`} onClick={() => installRelease(update, INSTALL_UPDATE)}>
              Update to {update.version}
            </ButtonItem>
          </PanelSectionRow>
        )}
        {status && (
          <PanelSectionRow>
            <Field
              label={providerLabel}
              description={
                status.provider.type === "none"
                  ? "Not connected"
                  : needsSignIn
                    ? "Signed out"
                    : status.ok
                      ? "Connected"
                      : status.error ?? "Not working right now"
              }
            />
          </PanelSectionRow>
        )}
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={busy || !running || !!daemon?.syncing}
            onClick={() => act(() => syncNow(0))}
          >
            {daemon?.syncing ? "Syncing…" : "Sync now"}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField
            label="Pause syncing"
            checked={paused}
            disabled={busy || !running}
            onChange={(value) => act(() => setPaused(value))}
          />
        </PanelSectionRow>
      </PanelSection>

      {needsSignIn && (
        <PanelSection title="Connect OneDrive">
          {signingIn ? (
            <>
              <SignIn
                onDone={() => {
                  setSigningIn(false);
                  refresh();
                }}
              />
              <PanelSectionRow>
                <ButtonItem layout="below" onClick={() => setSigningIn(false)}>
                  Cancel
                </ButtonItem>
              </PanelSectionRow>
            </>
          ) : (
            <PanelSectionRow>
              <ButtonItem layout="below" onClick={() => setSigningIn(true)}>
                Sign in with a phone
              </ButtonItem>
            </PanelSectionRow>
          )}
        </PanelSection>
      )}

      <PanelSection title="Games">
        <PanelSectionRow>
          <TextField
            placeholder="Search games"
            value={query}
            bShowClearAction
            onChange={(e) => setQuery(e.target.value)}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField label="Only with saves here" checked={onlyLocal} onChange={toggleOnlyLocal} />
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField label="Only without Steam Cloud" checked={onlyNoCloud} onChange={toggleOnlyNoCloud} />
        </PanelSectionRow>
        {!games && !error && (
          <PanelSectionRow>
            <Spinner style={{ width: 24 }} />
          </PanelSectionRow>
        )}
        {games?.length === 0 && (
          <PanelSectionRow>
            <Field label="No installed games found" />
          </PanelSectionRow>
        )}
        {!!games?.length && shown.length === 0 && (
          <PanelSectionRow>
            <Field
              label={query.trim() ? "No games match your search" : "No games match these filters"}
              description={onlyLocal && !query.trim() ? "Turn off “Only with saves here” to see every game" : undefined}
            />
          </PanelSectionRow>
        )}
        {shown.map((g) => {
          const syncingThis = daemon?.syncing && (daemon.syncing_game === g.app_id || daemon.syncing_game == null);
          return (
            <PanelSectionRow key={g.app_id}>
              <ButtonItem
                layout="below"
                description={
                  syncingThis
                    ? phaseText(daemon)
                    : g.cloud_only
                      ? `Not installed · from ${g.latest_machine ?? "another device"}`
                    : [
                        g.local_saves === false && "No saves here",
                        shortMode(g),
                        g.synced_at && `synced ${timeAgo(g.synced_at)}`,
                      ]
                        .filter(Boolean)
                        .join(" · ")
                }
                onClick={() => {
                  Navigation.Navigate(`/yasgm/game/${g.app_id}`);
                  Navigation.CloseSideMenus();
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 10, width: "100%", textAlign: "left" }}>
                  <GameIcon game={g} icons={icons} />
                  <span style={{ flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                    {g.game}
                  </span>
                  {syncingThis ? (
                    <Spinner style={{ width: 16, height: 16, flexShrink: 0 }} />
                  ) : (
                    g.steam_cloud && <FaCloud title="Steam Cloud already syncs this game" style={{ flexShrink: 0, opacity: 0.6 }} />
                  )}
                </div>
              </ButtonItem>
            </PanelSectionRow>
          );
        })}
      </PanelSection>

      {connected && (
        <PanelSection title="Cloud">
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              disabled={busy}
              onClick={() =>
                showModal(
                  <ConfirmModal
                    strTitle="Disconnect?"
                    strDescription={
                      status?.provider.type === "local"
                        ? "YASGM stops using this folder. Its contents are untouched."
                        : "YASGM signs out of OneDrive on this Deck and stops syncing. Your saves in OneDrive are untouched."
                    }
                    strOKButtonText="Disconnect"
                    onOK={() => act(disconnectCloud, "Disconnected")}
                  />,
                )
              }
            >
              Disconnect
            </ButtonItem>
          </PanelSectionRow>
        </PanelSection>
      )}

      <PanelSection title="About">
        <PanelSectionRow>
          <Field label="Version">{updates?.current ?? "…"}</Field>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem layout="below" description={updateNote ?? undefined} onClick={checkNow}>
            Check for updates
          </ButtonItem>
        </PanelSectionRow>
        {updates?.previous && (
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              description="Go back if this version misbehaves"
              onClick={() => installRelease(updates.previous!, INSTALL_DOWNGRADE)}
            >
              Reinstall {updates.previous.version}
            </ButtonItem>
          </PanelSectionRow>
        )}
      </PanelSection>
    </>
  );
}

// ---- per-game page --------------------------------------------------------------

function GamePage() {
  const { appId } = useParams<{ appId: string }>();
  const id = Number(appId);
  const { games, cloud, reload } = useGameLists();
  const game = useMemo(() => [...(games ?? []), ...cloud].find((g) => g.app_id === id) ?? null, [games, cloud, id]);
  const [versions, setVersions] = useState<Version[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const icons = useIcons();

  const load = useCallback(async () => {
    try {
      const list = await getVersions(id);
      setVersions(list.sort((a, b) => b.created - a.created));
      setError(null);
    } catch (err) {
      setError(errorText(err));
    }
  }, [id]);

  useEffect(() => {
    load();
  }, [id]);
  // A finished sync may have added a version.
  const daemon = useDaemonState(() => {
    load();
    reload(true);
  });
  const syncingThis = !!daemon?.syncing && (daemon.syncing_game === id || daemon.syncing_game == null);

  const act = async (action: () => Promise<string>) => {
    setBusy(true);
    try {
      const result = await action();
      toaster.toast({ title: "YASGM", body: result || "Done" });
    } catch (err) {
      toaster.toast({ title: "YASGM", body: errorText(err) });
    } finally {
      setBusy(false);
      load();
      reload(true);
    }
  };

  const confirmRestore = (v: Version) =>
    showModal(
      <ConfirmModal
        strTitle="Restore this version?"
        strDescription={`Saves from ${new Date(v.created * 1000).toLocaleString()} (${v.machine}) replace the ones on this Deck. Your current saves are kept as a pinned version first.`}
        strOKButtonText="Restore"
        onOK={() => act(() => restoreVersion(id, v.id))}
      />,
    );

  return (
    <div style={{ marginTop: 40, padding: "16px 24px", height: "calc(100% - 40px)", overflowY: "auto" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
        {game && <GameIcon game={game} icons={icons} size={40} />}
        <div style={{ minWidth: 0 }}>
          <h2 style={{ margin: "0 0 4px" }}>{game?.game ?? `App ${appId}`}</h2>
          {game && (
            <div style={muted}>
              {game.effective_mode}
              {game.steam_cloud && " · Steam Cloud already syncs this game"}
              {game.local_saves === false && !game.cloud_only && " · no saves on this Deck yet"}
              {game.cloud_only && " — install it to restore these saves"}
            </div>
          )}
        </div>
      </div>
      {syncingThis && (
        <div style={{ margin: "12px 0" }}>
          <ProgressBarWithInfo indeterminate sOperationText={phaseText(daemon)} />
        </div>
      )}
      {game?.cross_os_note && (
        <div style={{ ...muted, color: "#e8b45f", margin: "8px 0" }}>Cross-platform restore: {game.cross_os_note}.</div>
      )}
      {error && <div style={{ color: "#ff6b6b", margin: "8px 0" }}>{error}</div>}

      {game && !game.cloud_only && (
        <Focusable style={{ display: "flex", gap: 12, margin: "16px 0" }}>
          <div style={{ flex: 1 }}>
            <DropdownItem
              label="Mode"
              rgOptions={MODES}
              selectedOption={game.mode}
              disabled={busy}
              onChange={(option) => act(() => setMode(id, option.data))}
            />
          </div>
          <div style={{ flex: 1 }}>
            <DropdownItem
              label="Keep versions"
              rgOptions={[
                { data: 0, label: `Default (${game.default_keep})` },
                ...KEEP_CHOICES.map((n) => ({ data: n, label: String(n) })),
              ]}
              selectedOption={game.keep ?? 0}
              disabled={busy}
              onChange={(option) => act(() => setKeep(id, option.data))}
            />
          </div>
          <DialogButton style={{ width: 200, alignSelf: "center" }} disabled={busy || syncingThis} onClick={() => act(() => syncNow(id))}>
            {syncingThis ? "Syncing…" : "Sync this game"}
          </DialogButton>
        </Focusable>
      )}

      <h3 style={{ margin: "16px 0 8px" }}>Versions</h3>
      {!versions && !error && <Spinner style={{ width: 24 }} />}
      {versions?.length === 0 && <div style={muted}>No versions yet.</div>}
      {versions?.map((v) => (
        <Focusable
          key={v.id}
          style={{ display: "flex", alignItems: "center", gap: 12, padding: "8px 0", borderTop: "1px solid #ffffff1a" }}
        >
          <div style={{ flex: 1, minWidth: 0 }}>
            <div>
              {new Date(v.created * 1000).toLocaleString()}
              {v.active && " · active"}
              {v.pinned && " · pinned"}
              {v.pending && " · offline, not uploaded yet"}
            </div>
            <div style={muted}>
              {v.machine} · {v.os} · {v.files} files · {humanBytes(v.size)}
            </div>
          </div>
          <DialogButton style={{ width: 120 }} disabled={busy || v.active || !!game?.cloud_only} onClick={() => confirmRestore(v)}>
            Restore
          </DialogButton>
          <DialogButton style={{ width: 100 }} disabled={busy} onClick={() => act(() => setPinned(id, v.id, !v.pinned))}>
            {v.pinned ? "Unpin" : "Pin"}
          </DialogButton>
        </Focusable>
      ))}
    </div>
  );
}

// ---- plugin -----------------------------------------------------------------------

export default definePlugin(() => {
  routerHook.addRoute("/yasgm/game/:appId", GamePage, { exact: true });

  // Steam's own start/stop notifications, forwarded to the daemon: more
  // reliable than its process detection, and immediate on exit. On a start
  // the backend pauses the game (nInstanceID is its process) while the
  // daemon pulls newer cloud saves.
  const lifetime = SteamClient.GameSessions.RegisterForAppLifetimeNotifications((n) => {
    gameEvent(n.unAppID, n.bRunning, n.nInstanceID);
  });

  // The daemon's sync results (relayed by the backend) as toasts.
  const syncEvents = addEventListener<[SyncEvent[]]>("yasgm_sync_events", toastEvents);
  const updateFound = addEventListener<[Release]>("yasgm_update", (release) => {
    toaster.toast({ title: "YASGM update available", body: `Version ${release.version} — install it from the YASGM panel` });
  });

  return {
    name: "YASGM",
    titleView: <div className={staticClasses.Title}>YASGM</div>,
    content: <Content />,
    icon: <FaCloudUploadAlt />,
    onDismount() {
      lifetime.unregister();
      removeEventListener("yasgm_sync_events", syncEvents);
      removeEventListener("yasgm_update", updateFound);
      routerHook.removeRoute("/yasgm/game/:appId");
    },
  };
});
