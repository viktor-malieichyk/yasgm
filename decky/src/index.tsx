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
  Spinner,
  ToggleField,
  showModal,
  staticClasses,
  useParams,
} from "@decky/ui";
import { addEventListener, callable, definePlugin, removeEventListener, routerHook, toaster } from "@decky/api";
import { useCallback, useEffect, useState } from "react";
import { FaCloudUploadAlt } from "react-icons/fa";
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
  watch?: { paused: boolean; running_game: number | null; pending_sync: number[]; watching_games: number };
  watch_error?: string;
}

/** One row of `yasgm games --json`. */
interface Game {
  app_id: number;
  game: string;
  mode: string;
  effective_mode: string;
  in_manifest: boolean;
  cross_os_note: string | null;
  last_version: string | null;
  synced_at: number | null;
  keep: number | null;
  default_keep: number;
}

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

const getStatus = callable<[], Status>("get_status");
const getGames = callable<[], Game[]>("get_games");
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
  const [games, setGames] = useState<Game[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [signingIn, setSigningIn] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [s, g] = await Promise.all([getStatus(), getGames()]);
      setStatus(s);
      setGames(g);
      setError(null);
    } catch (err) {
      setError(errorText(err));
    }
  }, []);

  useEffect(() => {
    refresh();
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

  const paused = status?.watch?.paused ?? false;
  // One provider at a time, and signing in is choosing it: no provider
  // switch. Disconnected (or signed out of OneDrive) → QR sign-in; connected
  // → Disconnect.
  const needsSignIn = status?.provider.type === "none" || (status?.provider.type === "onedrive" && !status.signed_in);
  const connected = !!status && !needsSignIn;
  const providerLabel = status?.provider.type === "local" ? `Folder: ${status.provider.path}` : "OneDrive";

  return (
    <>
      <PanelSection title="Status">
        {!status && !error && (
          <PanelSectionRow>
            <Spinner style={{ width: 24 }} />
          </PanelSectionRow>
        )}
        {error && (
          <PanelSectionRow>
            <Field label="Something went wrong" description={error} />
          </PanelSectionRow>
        )}
        {status && (
          <>
            <PanelSectionRow>
              <Field
                label={providerLabel}
                description={
                  status.provider.type === "none"
                    ? "Not connected — your saves aren't being backed up"
                    : needsSignIn
                      ? "Signed out"
                      : status.ok
                        ? "Connected"
                        : status.error ?? "Not working right now"
                }
              />
            </PanelSectionRow>
            <PanelSectionRow>
              <Field
                label="Sync daemon"
                description={
                  !status.daemon.running
                    ? "Not running"
                    : paused
                      ? "Paused"
                      : status.watch?.pending_sync.length
                        ? `Waiting to sync ${status.watch.pending_sync.length} game(s)`
                        : `Watching ${status.watch?.watching_games ?? 0} games`
                }
              />
            </PanelSectionRow>
            <PanelSectionRow>
              <ButtonItem layout="below" disabled={busy || !status.daemon.running} onClick={() => act(() => syncNow(0), "Sync started")}>
                Sync now
              </ButtonItem>
            </PanelSectionRow>
            <PanelSectionRow>
              <ToggleField
                label="Pause syncing"
                checked={paused}
                disabled={busy || !status.daemon.running}
                onChange={(value) => act(() => setPaused(value))}
              />
            </PanelSectionRow>
          </>
        )}
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

      <PanelSection title="Games">
        {games?.length === 0 && (
          <PanelSectionRow>
            <Field label="No installed games found" />
          </PanelSectionRow>
        )}
        {games?.map((g) => (
          <PanelSectionRow key={g.app_id}>
            <ButtonItem
              layout="below"
              description={
                g.synced_at ? `${shortMode(g)} · synced ${timeAgo(g.synced_at)}` : shortMode(g)
              }
              onClick={() => {
                Navigation.Navigate(`/yasgm/game/${g.app_id}`);
                Navigation.CloseSideMenus();
              }}
            >
              {g.game}
            </ButtonItem>
          </PanelSectionRow>
        ))}
      </PanelSection>
    </>
  );
}

// ---- per-game page --------------------------------------------------------------

function GamePage() {
  const { appId } = useParams<{ appId: string }>();
  const id = Number(appId);
  const [game, setGame] = useState<Game | null>(null);
  const [versions, setVersions] = useState<Version[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const games = await getGames();
      setGame(games.find((g) => g.app_id === id) ?? null);
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
      <h2 style={{ margin: "0 0 4px" }}>{game?.game ?? `App ${appId}`}</h2>
      {game && <div style={muted}>{game.effective_mode}</div>}
      {game?.cross_os_note && (
        <div style={{ ...muted, color: "#e8b45f", margin: "8px 0" }}>Cross-platform restore: {game.cross_os_note}.</div>
      )}
      {error && <div style={{ color: "#ff6b6b", margin: "8px 0" }}>{error}</div>}

      {game && (
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
          <DialogButton style={{ width: 200, alignSelf: "center" }} disabled={busy} onClick={() => act(() => syncNow(id))}>
            Sync this game
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
          <DialogButton style={{ width: 120 }} disabled={busy || v.active} onClick={() => confirmRestore(v)}>
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
  const syncEvents = addEventListener<[SyncEvent]>("yasgm_sync_event", (e) => {
    toaster.toast({ title: "YASGM", body: eventToast(e) });
  });

  return {
    name: "YASGM",
    titleView: <div className={staticClasses.Title}>YASGM</div>,
    content: <Content />,
    icon: <FaCloudUploadAlt />,
    onDismount() {
      lifetime.unregister();
      removeEventListener("yasgm_sync_event", syncEvents);
      routerHook.removeRoute("/yasgm/game/:appId");
    },
  };
});
