<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import logo from "./assets/logo.png";

  type SyncStatus = {
    state: "off" | "syncing" | "idle" | "error";
    message: string | null;
    last_synced: number | null;
    last: { downloaded: number; uploaded: number; removed: number };
  };
  type OcrStatus = { state: "idle" | "preparing" | "reading" | "unavailable"; done: number; total: number; message: string | null };
  type Status = {
    stage: "setup" | "signin" | "folder" | "ready";
    account: string | null;
    drive_folder: { id: string; name: string } | null;
    local_folder: string | null;
    count: number;
    text_indexed: number;
    shortcut: string;
    shortcut_error: string | null;
    sync: SyncStatus;
    ocr: OcrStatus;
    autostart: boolean;
    primary_action: "paste" | "copy";
    version: string;
  };

  const sections = [
    { id: "general", label: "General" },
    { id: "account", label: "Account & Sync" },
    { id: "text", label: "Text Search" },
    { id: "duplicates", label: "Duplicates" },
    { id: "about", label: "About" },
  ] as const;
  type Section = (typeof sections)[number]["id"];

  const isMac = navigator.userAgent.includes("Mac");

  let status = $state<Status | null>(null);
  let section = $state<Section>("general");
  let error = $state<string | null>(null);
  let busy = $state(false);
  let now = $state(Date.now());

  // Shortcut recorder
  let recording = $state(false);
  let preview = $state<string[]>([]);
  let confirmReread = $state(false);

  async function refresh() {
    status = await invoke<Status>("get_status");
  }

  async function act(fn: () => Promise<unknown>) {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await fn();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  // ---------- shortcut recorder ----------

  const MODIFIER_CODES = ["Control", "Alt", "Shift", "Meta", "OS"];

  function modifierLabels(e: KeyboardEvent) {
    const parts: string[] = [];
    if (e.ctrlKey) parts.push("Ctrl");
    if (e.altKey) parts.push(isMac ? "Option" : "Alt");
    if (e.shiftKey) parts.push("Shift");
    if (e.metaKey) parts.push(isMac ? "Cmd" : "Win");
    return parts;
  }

  function keyLabel(code: string) {
    if (code.startsWith("Key")) return code.slice(3);
    if (code.startsWith("Digit")) return code.slice(5);
    const arrows: Record<string, string> = { ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→" };
    return arrows[code] ?? code;
  }

  function startRecording() {
    error = null;
    preview = [];
    recording = true;
  }

  function onKeydown(e: KeyboardEvent) {
    // Ctrl+W / Cmd+W closes Settings, like any normal window.
    if (!recording && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "w") {
      e.preventDefault();
      getCurrentWindow().close();
      return;
    }
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();

    if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.metaKey) {
      recording = false;
      preview = [];
      return;
    }
    const mods = modifierLabels(e);
    if (MODIFIER_CODES.some((m) => e.code.startsWith(m))) {
      preview = mods; // still holding modifiers
      return;
    }
    preview = [...mods, keyLabel(e.code)];
    if (!e.ctrlKey && !e.altKey && !e.metaKey) {
      error = `Include ${isMac ? "Ctrl, Option or Cmd" : "Ctrl, Alt or Win"} so the shortcut doesn't clash with typing.`;
      return;
    }
    const accelerator = [
      e.ctrlKey && "control",
      e.altKey && "alt",
      e.shiftKey && "shift",
      e.metaKey && "super",
      e.code,
    ]
      .filter(Boolean)
      .join("+");
    recording = false;
    act(async () => {
      status = await invoke<Status>("set_shortcut", { accelerator });
    });
  }

  // ---------- actions ----------

  const setPrimaryAction = (action: "paste" | "copy") =>
    act(async () => {
      status = await invoke<Status>("set_primary_action", { action });
    });

  const toggleAutostart = () =>
    act(async () => {
      status = await invoke<Status>("set_autostart", { enabled: !status?.autostart });
    });

  const resetShortcut = () =>
    act(async () => {
      status = await invoke<Status>("reset_shortcut");
    });

  const signIn = () =>
    act(async () => {
      await invoke("sign_in");
      await refresh();
    });

  const signOut = () =>
    act(async () => {
      status = await invoke<Status>("sign_out_cmd");
    });

  const reread = () =>
    act(async () => {
      confirmReread = false;
      await invoke("reread_all_text");
    });

  const openLink = (target: string) => act(() => invoke("open_link", { target }));

  // ---------- duplicates ----------

  type DupItem = {
    path: string;
    name: string;
    rel: string;
    width: number | null;
    height: number | null;
    version: number;
    bytes: number;
  };
  type DupGroup = { exact: boolean; items: DupItem[] };
  type Duplicates = { groups: DupGroup[]; pending: number };

  let dupes = $state<Duplicates | null>(null);
  let dupesLoading = $state(false);
  let deleted = $state<{ name: string; token: string } | null>(null);
  let deletedTimer: ReturnType<typeof setTimeout> | undefined;

  async function loadDupes() {
    dupesLoading = true;
    try {
      dupes = await invoke<Duplicates>("find_duplicates");
    } catch (e) {
      error = String(e);
    } finally {
      dupesLoading = false;
    }
  }

  $effect(() => {
    if (section === "duplicates" && !dupes && !dupesLoading) loadDupes();
  });

  const deleteDupe = (item: DupItem) =>
    act(async () => {
      const token = await invoke<string>("delete_meme", { path: item.path });
      clearTimeout(deletedTimer);
      deleted = { name: item.name, token };
      deletedTimer = setTimeout(() => (deleted = null), 10_000);
      await loadDupes();
    });

  const undoDupe = () =>
    act(async () => {
      if (!deleted) return;
      const token = deleted.token;
      deleted = null;
      await invoke("undo_delete", { token });
      await loadDupes();
    });

  const thumb = (item: DupItem) => `${convertFileSrc(item.path, "thumb")}?v=${item.version}`;

  function size(bytes: number) {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  }

  // ---------- formatting ----------

  function ago(ms: number) {
    const s = Math.max(0, Math.round((now - ms) / 1000));
    if (s < 10) return "just now";
    if (s < 60) return `${s} seconds ago`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m} minute${m === 1 ? "" : "s"} ago`;
    const h = Math.round(m / 60);
    return `${h} hour${h === 1 ? "" : "s"} ago`;
  }

  function syncText(s: SyncStatus) {
    if (s.state === "syncing") return "Syncing…";
    if (s.state === "error") return "Last sync had a problem";
    if (s.state === "off" || !s.last_synced) return "Not synced yet";
    return `Synced ${ago(s.last_synced)}`;
  }

  onMount(() => {
    refresh();
    const unlisten = ["sync-status", "ocr-status", "library-changed"].map((e) => listen(e, () => refresh()));
    const timer = setInterval(() => (now = Date.now()), 5000);
    return () => {
      clearInterval(timer);
      unlisten.forEach((p) => p.then((f) => f()));
    };
  });
</script>

<svelte:window onkeydown={onKeydown} />

<div class="settings">
  <nav class="side" aria-label="Settings sections">
    <div class="brand"><img src={logo} alt="" width="22" height="22" />Settings</div>
    {#each sections as s (s.id)}
      <button class:active={section === s.id} onclick={() => (section = s.id)}>{s.label}</button>
    {/each}
  </nav>

  <main class="content">
    {#if error}
      <div class="alert" role="alert">
        <span>{error}</span>
        <button class="x" aria-label="Dismiss" onclick={() => (error = null)}>×</button>
      </div>
    {/if}

    {#if !status}
      <p class="muted">Loading…</p>
    {:else if section === "general"}
      <h1>General</h1>

      <div class="card">
        <div class="row">
          <div>
            <h3>Open memeity</h3>
            <p>Shortcut that opens the search pane from anywhere.</p>
          </div>
          <button class="recorder" class:recording onclick={startRecording} disabled={busy}>
            {#if recording}
              {#if preview.length}
                {#each preview as k, i (i)}<kbd>{k}</kbd>{/each}
              {:else}
                <span class="hint">Press keys…</span>
              {/if}
            {:else}
              {#each status.shortcut.split("+") as k, i (i)}<kbd>{k}</kbd>{/each}
            {/if}
          </button>
        </div>
        {#if recording}
          <p class="note">Press the new combination, or Esc to cancel.</p>
        {:else if status.shortcut_error}
          <p class="note warn">{status.shortcut_error}</p>
        {/if}
        <div class="row tight">
          <span></span>
          <button class="btn ghost sm" onclick={resetShortcut} disabled={busy || recording}>Reset to default</button>
        </div>
      </div>

      <div class="card">
        <div class="row">
          <div>
            <h3>Enter key</h3>
            <p>What Enter (or a click) does with the selected meme. Shift+Enter does the other.</p>
          </div>
          <div class="segmented" role="radiogroup" aria-label="Enter key action">
            <button
              role="radio"
              aria-checked={status.primary_action === "paste"}
              class:on={status.primary_action === "paste"}
              onclick={() => setPrimaryAction("paste")}
              disabled={busy}
            >Paste to App</button>
            <button
              role="radio"
              aria-checked={status.primary_action === "copy"}
              class:on={status.primary_action === "copy"}
              onclick={() => setPrimaryAction("copy")}
              disabled={busy}
            >Copy to Clipboard</button>
          </div>
        </div>
      </div>

      <div class="card">
        <div class="row">
          <div>
            <h3>Launch at login</h3>
            <p>Start memeity quietly in the {isMac ? "menu bar" : "tray"} when you log in.</p>
          </div>
          <button
            class="switch"
            role="switch"
            aria-checked={status.autostart}
            aria-label="Launch at login"
            onclick={toggleAutostart}
            disabled={busy}
          ><span></span></button>
        </div>
      </div>
    {:else if section === "account"}
      <h1>Account &amp; Sync</h1>

      <div class="card">
        {#if status.account}
          <div class="row">
            <div>
              <h3>{status.account}</h3>
              <p>Signed in to Google Drive</p>
            </div>
            <button class="btn outline sm danger" onclick={signOut} disabled={busy}>Sign out</button>
          </div>
        {:else}
          <div class="row">
            <div>
              <h3>Not signed in</h3>
              <p>Sign in to sync your memes through Google Drive.</p>
            </div>
            <button class="btn primary sm" onclick={signIn} disabled={busy || status.stage === "setup"}>Sign in with Google</button>
          </div>
        {/if}
      </div>

      {#if status.drive_folder}
        <div class="card">
          <div class="row">
            <div>
              <h3>Sync</h3>
              <p class:warn={status.sync.state === "error"} title={status.sync.message ?? ""}>
                {syncText(status.sync)}{status.sync.state === "error" && status.sync.message ? `: ${status.sync.message}` : ""}
              </p>
            </div>
            <button class="btn outline sm" onclick={() => act(() => invoke("sync_now"))} disabled={busy || status.sync.state === "syncing"}>
              Sync now
            </button>
          </div>
          <div class="divider"></div>
          <div class="row">
            <div>
              <h3>Memes folder</h3>
              <p class="mono" title={status.local_folder ?? ""}>{status.local_folder}</p>
            </div>
            <div class="buttons">
              <button class="btn outline sm" onclick={() => act(() => invoke("open_folder"))}>Open folder</button>
              <button class="btn outline sm" onclick={() => openLink("drive")}>Open in Drive</button>
            </div>
          </div>
          <p class="note">
            {status.count} meme{status.count === 1 ? "" : "s"}. Add memes by dropping them into this folder on any device. Files added
            through Drive's website don't sync, because memeity can only see files it created.
          </p>
        </div>
      {/if}
    {:else if section === "text"}
      <h1>Text Search</h1>

      <div class="card">
        <div class="row">
          <div>
            <h3>Text in images</h3>
            {#if status.ocr.state === "preparing"}
              <p>{status.ocr.message ?? "Getting text recognition ready…"}</p>
            {:else if status.ocr.state === "reading"}
              <p>Reading text… {status.ocr.done} of {status.ocr.total}</p>
            {:else if status.ocr.state === "unavailable"}
              <p class="warn">{status.ocr.message ?? "Text recognition isn't available on this device."}</p>
            {:else}
              <p>Text has been read for {status.text_indexed} of {status.count} memes.</p>
            {/if}
          </div>
        </div>
        {#if status.ocr.state === "reading" && status.ocr.total > 0}
          <div class="progress"><div style:width="{(status.ocr.done / status.ocr.total) * 100}%"></div></div>
        {/if}
        <p class="note">
          memeity reads the words in your memes with PaddleOCR, an AI text-recognition model that runs entirely on this
          device (downloaded once, about 30 MB). Images never leave your computer for this. Results sync to your other
          devices, so each meme is only read once.
        </p>
      </div>

      <div class="card">
        <div class="row">
          <div>
            <h3>Re-read all memes</h3>
            <p>Run text recognition again on every meme. Memes read by an older version are re-read automatically.</p>
          </div>
          {#if confirmReread}
            <div class="buttons">
              <button class="btn ghost sm" onclick={() => (confirmReread = false)}>Cancel</button>
              <button class="btn primary sm" onclick={reread} disabled={busy}>Re-read {status.count}</button>
            </div>
          {:else}
            <button class="btn outline sm" onclick={() => (confirmReread = true)} disabled={busy || status.ocr.state === "reading" || status.count === 0}>
              Re-read…
            </button>
          {/if}
        </div>
      </div>
    {:else if section === "duplicates"}
      <div class="title-row">
        <h1>Duplicates</h1>
        <button class="btn outline sm" onclick={loadDupes} disabled={dupesLoading}>
          {dupesLoading ? "Checking…" : "Check again"}
        </button>
      </div>

      {#if deleted}
        <div class="undo-bar" role="status">
          <span>Deleted {deleted.name}</span>
          <button class="btn outline sm" onclick={undoDupe} disabled={busy}>Undo</button>
        </div>
      {/if}

      {#if dupes && dupes.pending > 0}
        <p class="note top">
          Still fingerprinting {dupes.pending} meme{dupes.pending === 1 ? "" : "s"} in the background. Look-alikes among
          them will show up once that's done.
        </p>
      {/if}

      {#if !dupes}
        <p class="muted">Looking for duplicates…</p>
      {:else if dupes.groups.length === 0}
        <div class="card">
          <h3>No duplicates</h3>
          <p class="muted small">Every meme in your library is one of a kind.</p>
        </div>
      {:else}
        <p class="note top">
          {dupes.groups.length} group{dupes.groups.length === 1 ? "" : "s"}. The first in each is the best copy (highest
          resolution). Deleting removes a meme from Drive and every device; you can undo for a few seconds.
        </p>
        {#each dupes.groups as group (group.items.map((i) => i.rel).join("|"))}
          <div class="card dupe-group">
            <p class="group-kind">{group.exact ? "Identical copies" : "Look the same"}</p>
            {#each group.items as item, n (item.rel)}
              <div class="dupe">
                <img src={thumb(item)} alt="" width="56" height="56" loading="lazy" />
                <div class="dupe-info">
                  <span class="dupe-name" title={item.rel}>{item.rel}</span>
                  <span class="muted small">
                    {item.width && item.height ? `${item.width}×${item.height} · ` : ""}{size(item.bytes)}
                  </span>
                </div>
                {#if n === 0}<span class="badge">Best copy</span>{/if}
                <button class="btn outline sm danger" onclick={() => deleteDupe(item)} disabled={busy}>Delete</button>
              </div>
            {/each}
          </div>
        {/each}
      {/if}
    {:else if section === "about"}
      <div class="about">
        <img src={logo} alt="memeity" width="72" height="72" />
        <h1>memeity</h1>
        <p class="muted">Version {status.version}</p>
        <div class="buttons center">
          <button class="btn outline sm" onclick={() => openLink("website")}>Website</button>
          <button class="btn outline sm" onclick={() => openLink("privacy")}>Privacy policy</button>
          <button class="btn outline sm" onclick={() => openLink("source")}>Source code</button>
        </div>
      </div>
    {/if}
  </main>
</div>

<style>
  .settings {
    display: flex;
    height: 100%;
  }

  /* ---------- sidebar ---------- */

  .side {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 190px;
    flex-shrink: 0;
    padding: 16px 10px;
    border-right: 1px solid var(--border);
    background: var(--bar);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 8px 14px;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .side button {
    padding: 7px 10px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }

  .side button:hover {
    color: var(--fg);
  }

  .side button.active {
    background: var(--accent);
    color: var(--fg);
    font-weight: 500;
  }

  /* ---------- content ---------- */

  .content {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 22px 26px 28px;
  }

  h1 {
    margin: 0 0 16px;
    font-size: 20px;
    font-weight: 600;
    letter-spacing: -0.025em;
  }

  .card {
    margin-bottom: 12px;
    padding: 14px 16px;
    border: 1px solid var(--border);
    border-radius: 10px;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
  }

  .row > div:first-child {
    min-width: 0;
  }

  .row.tight {
    margin-top: 8px;
  }

  h3 {
    margin: 0 0 2px;
    font-size: 14px;
    font-weight: 500;
  }

  .row p {
    margin: 0;
    color: var(--muted-fg);
    font-size: 13px;
  }

  .note {
    margin: 10px 0 0;
    color: var(--muted-fg);
    font-size: 12px;
  }

  .warn {
    color: var(--destructive) !important;
  }

  .muted {
    color: var(--muted-fg);
  }

  .mono {
    font: 12px "Geist Mono Variable", ui-monospace, monospace;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .divider {
    height: 1px;
    margin: 12px 0;
    background: var(--border);
  }

  .buttons {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }

  .buttons.center {
    justify-content: center;
  }

  .alert {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    margin-bottom: 14px;
    padding: 10px 12px;
    border: 1px solid color-mix(in srgb, var(--destructive) 40%, var(--border));
    border-radius: 8px;
    background: color-mix(in srgb, var(--destructive) 12%, var(--bg));
    color: var(--destructive);
    font-size: 13px;
    user-select: text;
    -webkit-user-select: text;
  }

  .alert span {
    flex: 1;
  }

  .x {
    border: none;
    background: none;
    color: var(--muted-fg);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
  }

  /* ---------- controls ---------- */

  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 32px;
    padding: 0 12px;
    border: 1px solid transparent;
    border-radius: 6px;
    font-size: 13px;
    font-weight: 500;
    white-space: nowrap;
    cursor: pointer;
  }

  .btn.sm {
    height: 30px;
  }

  .btn.primary {
    background: var(--primary);
    color: var(--primary-fg);
  }

  .btn.outline {
    border-color: var(--border);
    background: var(--bg);
    color: var(--fg);
  }

  .btn.outline:hover:not(:disabled),
  .btn.ghost:hover:not(:disabled) {
    background: var(--accent);
  }

  .btn.ghost {
    background: none;
    color: var(--muted-fg);
  }

  .btn.danger {
    color: var(--destructive);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .recorder {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    min-width: 150px;
    height: 34px;
    padding: 0 10px;
    justify-content: center;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    cursor: pointer;
    flex-shrink: 0;
  }

  .recorder:hover:not(:disabled) {
    border-color: var(--border-strong);
  }

  .recorder.recording {
    border-color: var(--ring);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--ring) 20%, transparent);
  }

  .recorder kbd {
    height: 22px;
    min-width: 22px;
    font-size: 12px;
  }

  .recorder .hint {
    color: var(--muted-fg);
    font-size: 13px;
  }

  .segmented {
    display: inline-flex;
    flex-shrink: 0;
    padding: 3px;
    border-radius: 8px;
    background: var(--muted);
  }

  .segmented button {
    height: 26px;
    padding: 0 10px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    cursor: pointer;
  }

  .segmented button.on {
    background: var(--bg);
    color: var(--fg);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.2);
  }

  .switch {
    position: relative;
    width: 40px;
    height: 22px;
    flex-shrink: 0;
    padding: 0;
    border: none;
    border-radius: 999px;
    background: var(--border-strong);
    cursor: pointer;
    transition: background 120ms ease;
  }

  .switch span {
    position: absolute;
    top: 3px;
    left: 3px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--bg);
    transition: transform 120ms ease;
  }

  .switch[aria-checked="true"] {
    background: var(--primary);
  }

  .switch[aria-checked="true"] span {
    transform: translateX(18px);
  }

  .progress {
    height: 4px;
    margin-top: 12px;
    border-radius: 999px;
    background: var(--muted);
    overflow: hidden;
  }

  .progress div {
    height: 100%;
    background: var(--primary);
    transition: width 200ms ease;
  }

  .title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 16px;
  }

  .title-row h1 {
    margin: 0;
  }

  .note.top {
    margin: 0 0 12px;
  }

  .small {
    font-size: 12px;
  }

  .undo-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
    padding: 8px 10px 8px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    font-size: 13px;
  }

  .dupe-group {
    padding: 10px 12px;
  }

  .group-kind {
    margin: 0 0 6px;
    color: var(--muted-fg);
    font-size: 11px;
    font-weight: 500;
  }

  .dupe {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 0;
  }

  .dupe + .dupe {
    border-top: 1px solid var(--border);
  }

  .dupe img {
    width: 56px;
    height: 56px;
    flex-shrink: 0;
    border-radius: 6px;
    object-fit: cover;
    background: var(--muted);
  }

  .dupe-info {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-width: 0;
  }

  .dupe-name {
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .badge {
    flex-shrink: 0;
    padding: 2px 8px;
    border: 1px solid var(--border);
    border-radius: 999px;
    color: var(--muted-fg);
    font-size: 11px;
    font-weight: 500;
  }

  .about {
    padding-top: 40px;
    text-align: center;
  }

  .about h1 {
    margin: 12px 0 2px;
  }

  .about .muted {
    margin: 0 0 18px;
    font-size: 13px;
  }
</style>
