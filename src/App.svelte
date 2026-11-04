<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, tick } from "svelte";
  import Setup from "./Setup.svelte";

  type Hit = { path: string; name: string; rel: string };
  type SearchResult = { hits: Hit[]; total: number; micros: number };
  type DriveFolder = { id: string; name: string };
  type SyncStatus = {
    state: "off" | "syncing" | "idle" | "error";
    message: string | null;
    last_synced: number | null;
    last: { downloaded: number; uploaded: number; removed: number };
  };
  type Status = {
    stage: "setup" | "signin" | "folder" | "ready";
    account: string | null;
    drive_folder: DriveFolder | null;
    local_folder: string | null;
    count: number;
    shortcut: string;
    shortcut_error: string | null;
    sync: SyncStatus;
    client_file: string | null;
  };

  const COLS = 5;
  const LIMIT = 200;

  let status = $state<Status | null>(null);
  let query = $state("");
  let error = $state<string | null>(null);
  let busy = $state(false);
  let now = $state(Date.now());

  // ready stage
  let hits = $state<Hit[]>([]);
  let total = $state(0);
  let micros = $state(0);
  let selected = $state(0);
  let confirmDelete = $state<Hit | null>(null);
  let searchSeq = 0;

  // folder stage
  let trail = $state<DriveFolder[]>([{ id: "root", name: "My Drive" }]);
  let folders = $state<DriveFolder[]>([]);
  let foldersLoading = $state(false);
  let folderSel = $state(0);

  let input: HTMLInputElement | undefined = $state();
  let grid: HTMLDivElement | undefined = $state();
  let list: HTMLDivElement | undefined = $state();

  const stage = $derived(status?.stage);
  const current = $derived(hits[selected]);
  const here = $derived(trail[trail.length - 1]);
  const NEW_FOLDER = "memeity";
  // Offer to create a "memeity" folder unless one already exists right here.
  const showCreate = $derived(
    !foldersLoading && !folders.some((f) => f.name.trim().toLowerCase() === NEW_FOLDER),
  );
  const rowOffset = $derived(showCreate ? 1 : 0);
  const shownFolders = $derived(
    query.trim() ? folders.filter((f) => f.name.toLowerCase().includes(query.trim().toLowerCase())) : folders,
  );

  // ---------- shared ----------

  async function refresh(resetSelection = false) {
    const prev = status?.stage;
    status = await invoke<Status>("get_status");
    if (status.stage === "ready") await runSearch(resetSelection);
    if (status.stage === "folder" && prev !== "folder") {
      query = "";
      trail = [{ id: "root", name: "My Drive" }];
      await loadFolders();
    }
  }

  async function act<T>(fn: () => Promise<T>) {
    if (busy) return;
    busy = true;
    error = null;
    try {
      return await fn();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  // ---------- setup / sign in ----------

  const loadClientFile = () =>
    act(async () => {
      status = await invoke<Status>("load_client_file");
    });

  const signIn = () =>
    act(async () => {
      status = await invoke<Status>("sign_in");
      await refresh(true);
    });

  // ---------- folder picker ----------

  async function loadFolders() {
    foldersLoading = true;
    folderSel = 0;
    error = null;
    try {
      folders = await invoke<DriveFolder[]>("list_drive_folders", { parent: here.id });
    } catch (e) {
      folders = [];
      error = String(e);
    } finally {
      foldersLoading = false;
    }
  }

  async function openFolder(f: DriveFolder) {
    trail = [...trail, f];
    query = "";
    await loadFolders();
  }

  async function goUp(toIndex = trail.length - 2) {
    if (toIndex < 0 || toIndex >= trail.length - 1) return;
    trail = trail.slice(0, toIndex + 1);
    query = "";
    await loadFolders();
  }

  const useFolder = (f: DriveFolder) =>
    act(async () => {
      if (f.id === "root") return;
      query = "";
      status = await invoke<Status>("set_drive_folder", { id: f.id, name: f.name });
      await runSearch(true);
    });

  /** Creates a "memeity" folder in the folder being viewed, and starts syncing it. */
  const createMemeityFolder = () =>
    act(async () => {
      const created = await invoke<DriveFolder>("create_drive_folder", { parent: here.id, name: NEW_FOLDER });
      query = "";
      status = await invoke<Status>("set_drive_folder", { id: created.id, name: created.name });
      await runSearch(true);
    });

  async function selectFolder(i: number) {
    const rows = shownFolders.length + rowOffset;
    if (rows === 0) return;
    folderSel = Math.max(0, Math.min(rows - 1, i));
    await tick();
    list?.querySelector(`[data-i="${folderSel}"]`)?.scrollIntoView({ block: "nearest" });
  }

  // ---------- search grid ----------

  async function runSearch(resetSelection: boolean) {
    const seq = ++searchSeq;
    const res = await invoke<SearchResult>("search", { query, limit: LIMIT });
    if (seq !== searchSeq) return; // a newer keystroke already won
    hits = res.hits;
    total = res.total;
    micros = res.micros;
    selected = resetSelection ? 0 : Math.min(selected, Math.max(hits.length - 1, 0));
  }

  async function select(i: number) {
    if (hits.length === 0) return;
    confirmDelete = null;
    selected = Math.max(0, Math.min(hits.length - 1, i));
    await tick();
    grid?.querySelector(`[data-i="${selected}"]`)?.scrollIntoView({ block: "nearest" });
  }

  const copy = (i = selected) =>
    act(async () => {
      const hit = hits[i];
      if (hit) await invoke("copy_meme", { path: hit.path }); // hides the pane on success
    });

  const doDelete = (hit: Hit) =>
    act(async () => {
      confirmDelete = null;
      await invoke("delete_meme", { path: hit.path });
      await refresh();
    });

  // ---------- keyboard ----------

  function onInput() {
    error = null;
    confirmDelete = null;
    if (stage === "ready") runSearch(true);
    else folderSel = 0;
  }

  function onKeydown(e: KeyboardEvent) {
    const key = e.key;
    const ctrl = e.ctrlKey || e.metaKey;

    if (key === "Escape") {
      e.preventDefault();
      if (confirmDelete) confirmDelete = null;
      else if (stage === "ready") invoke("hide"); // the setup window doesn't vanish on Esc
      return;
    }

    if (stage === "setup") {
      // Setup.svelte handles arrows/Enter for its own steps.
      if (ctrl && key.toLowerCase() === "o") {
        e.preventDefault();
        loadClientFile();
      }
    } else if (stage === "signin") {
      if (key === "Enter") {
        e.preventDefault();
        signIn();
      }
    } else if (stage === "folder") {
      if (key === "ArrowDown") { e.preventDefault(); selectFolder(folderSel + 1); }
      else if (key === "ArrowUp") { e.preventDefault(); selectFolder(folderSel - 1); }
      else if (key === "Enter" && ctrl) { e.preventDefault(); useFolder(here); }
      else if (key === "Enter") {
        e.preventDefault();
        if (showCreate && folderSel === 0) createMemeityFolder();
        else { const f = shownFolders[folderSel - rowOffset]; if (f) openFolder(f); }
      }
      else if (key === "Backspace" && query === "") { e.preventDefault(); goUp(); }
    } else if (stage === "ready") {
      if (confirmDelete) {
        if (key === "Enter") { e.preventDefault(); doDelete(confirmDelete); }
        return;
      }
      switch (key) {
        case "ArrowRight": e.preventDefault(); select(selected + 1); break;
        case "ArrowLeft":  e.preventDefault(); select(selected - 1); break;
        case "ArrowDown":  e.preventDefault(); select(selected + COLS); break;
        case "ArrowUp":    e.preventDefault(); select(selected - COLS); break;
        case "PageDown":   e.preventDefault(); select(selected + COLS * 3); break;
        case "PageUp":     e.preventDefault(); select(selected - COLS * 3); break;
        case "Enter":      e.preventDefault(); copy(); break;
        case "Delete":
          if (ctrl && current) { e.preventDefault(); confirmDelete = current; }
          break;
      }
    }
  }

  // ---------- lifecycle ----------

  onMount(() => {
    const unlisten = [
      listen("pane-shown", () => {
        error = null;
        confirmDelete = null;
        input?.focus();
        input?.select(); // typing replaces the last query, arrows keep it
        refresh();
      }),
      listen("library-changed", () => refresh()),
      listen("sync-status", () => refresh()),
    ];
    const timer = setInterval(() => (now = Date.now()), 5000);
    refresh(true).then(() => input?.focus());
    return () => {
      clearInterval(timer);
      unlisten.forEach((p) => p.then((f) => f()));
    };
  });

  function formatTime(us: number) {
    return us < 1000 ? `${us}µs` : `${(us / 1000).toFixed(1)}ms`;
  }

  function ago(ms: number) {
    const s = Math.max(0, Math.round((now - ms) / 1000));
    if (s < 10) return "just now";
    if (s < 60) return `${s}s ago`;
    const m = Math.round(s / 60);
    return m < 60 ? `${m}m ago` : `${Math.round(m / 60)}h ago`;
  }

  function syncLabel(s: SyncStatus) {
    if (s.state === "syncing") return "Syncing…";
    if (s.state === "off" || !s.last_synced) return "Not synced yet";
    const { downloaded, uploaded, removed } = s.last;
    const moved = [downloaded && `↓${downloaded}`, uploaded && `↑${uploaded}`, removed && `−${removed}`]
      .filter(Boolean)
      .join(" ");
    return `Synced ${ago(s.last_synced)}${moved ? ` · ${moved}` : ""}`;
  }
</script>

<svelte:window onkeydown={onKeydown} />

<main>
  {#if stage === "folder" || stage === "ready"}
    <header>
      <svg class="glass" viewBox="0 0 24 24" aria-hidden="true">
        <circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" />
      </svg>
      <input
        bind:this={input}
        bind:value={query}
        oninput={onInput}
        placeholder={stage === "folder" ? `Filter folders in ${here.name}…` : "Search memes…"}
        spellcheck="false"
        autocomplete="off"
      />
      {#if stage === "ready" && status?.drive_folder}
        <span class="chip" title={status.local_folder ?? ""}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" /></svg>
          {status.drive_folder.name}
        </span>
      {/if}
    </header>
  {/if}

  {#if status?.shortcut_error}
    <div class="banner">{status.shortcut_error}</div>
  {/if}

  <section class="body">
    {#if !status}
      <div class="empty"></div>
    {:else if stage === "setup"}
      <Setup onLoad={loadClientFile} {busy} />
    {:else if stage === "signin"}
      <div class="card center">
        <h2>Sign in to Google Drive</h2>
        <p>memeity syncs one Drive folder to this device. Your browser opens to sign in, and the token stays in Windows Credential Manager.</p>
        <p class="heads-up">
          Google will say <b>“Google hasn't verified this app”</b>. That's your own project, so click <b>Continue</b>, then allow Drive access.
        </p>
        <button class="primary" onclick={signIn} disabled={busy}>
          {busy ? "Waiting for your browser…" : "Sign in with Google"} {#if !busy}<kbd>↵</kbd>{/if}
        </button>
      </div>
    {:else if stage === "folder"}
      <div class="picker">
        <nav class="trail">
          {#each trail as crumb, i (crumb.id)}
            {#if i > 0}<span class="sep">›</span>{/if}
            <button class="crumb" disabled={i === trail.length - 1} onclick={() => goUp(i)}>{crumb.name}</button>
          {/each}
        </nav>
        <div class="list" bind:this={list}>
          {#if foldersLoading}
            <p class="muted pad">Loading folders…</p>
          {:else}
            {#if showCreate}
              <button
                class="row create"
                class:selected={folderSel === 0}
                data-i={0}
                tabindex="-1"
                disabled={busy}
                onmousedown={(e) => e.preventDefault()}
                onclick={() => (folderSel = 0)}
                ondblclick={createMemeityFolder}
              >
                <span class="plus" aria-hidden="true">+</span>
                <span>Create a “{NEW_FOLDER}” folder in {here.name} and use it</span>
                <span class="go">↵</span>
              </button>
            {/if}
            {#if shownFolders.length === 0}
              <p class="muted pad">{query.trim() ? "No folders match." : "No sub-folders here."}</p>
            {/if}
            {#each shownFolders as f, i (f.id)}
              <button
                class="row"
                class:selected={i + rowOffset === folderSel}
                data-i={i + rowOffset}
                tabindex="-1"
                onmousedown={(e) => e.preventDefault()}
                onclick={() => (folderSel = i + rowOffset)}
                ondblclick={() => openFolder(f)}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" /></svg>
                <span>{f.name}</span>
                <span class="go">›</span>
              </button>
            {/each}
          {/if}
        </div>
      </div>
    {:else if stage === "ready"}
      {#if status.count === 0}
        <div class="card center">
          {#if status.sync.state === "syncing"}
            <h2>Syncing from Google Drive…</h2>
            <p>Your memes will appear here as they download.</p>
          {:else}
            <h2>No memes in “{status.drive_folder?.name}” yet</h2>
            <p>Add .png, .jpg, .gif, .webp or .bmp files to the folder on Google Drive, or drop them into the local folder and they'll upload.</p>
            <button onclick={() => invoke("open_folder")}>Open local folder</button>
          {/if}
        </div>
      {:else if hits.length === 0}
        <div class="card center"><p>No memes match “{query}”.</p></div>
      {:else}
        <div class="grid" bind:this={grid}>
          {#each hits as hit, i (hit.path)}
            <button
              class="tile"
              class:selected={i === selected}
              class:doomed={confirmDelete?.path === hit.path}
              data-i={i}
              tabindex="-1"
              title={hit.rel}
              onmousedown={(e) => e.preventDefault()}
              onclick={() => select(i)}
              ondblclick={() => copy(i)}
            >
              <div class="thumb">
                <img src={convertFileSrc(hit.path)} alt="" loading="lazy" decoding="async" draggable="false" />
              </div>
              <span class="name">{hit.name}</span>
            </button>
          {/each}
        </div>
      {/if}
    {/if}
  </section>

  <footer>
    {#if confirmDelete}
      <span class="danger-text">Delete “{confirmDelete.name}” from Google Drive and every device?</span>
      <span class="spacer"></span>
      <span class="hint"><kbd>↵</kbd> Delete</span>
      <span class="hint"><kbd>esc</kbd> Cancel</span>
    {:else}
      {#if error}
        <span class="error" title={error}>{error}</span>
      {:else if stage === "ready" && current}
        <span class="current">{current.rel}</span>
      {:else if stage === "folder"}
        <span class="muted">Pick the Drive folder that holds your memes</span>
      {:else if status?.account}
        <span class="muted">{status.account}</span>
      {:else}
        <span class="muted">memeity</span>
      {/if}
      <span class="spacer"></span>

      {#if stage === "ready" && status}
        <span
          class="sync"
          class:err={status.sync.state === "error"}
          class:spin={status.sync.state === "syncing"}
          title={status.sync.message ?? status.account ?? ""}
        >
          {status.sync.state === "error" ? "Sync problem" : syncLabel(status.sync)}
        </span>
        <span class="muted stats">
          {query.trim() ? `${total}/${status.count}` : status.count} · {formatTime(micros)}
        </span>
        <span class="hint"><kbd>↵</kbd> Copy</span>
        <span class="hint"><kbd>ctrl del</kbd> Delete</span>
        <span class="hint"><kbd>esc</kbd> Close</span>
      {:else if stage === "folder"}
        {#if trail.length > 1}<span class="hint"><kbd>⌫</kbd> Up</span>{/if}
        <span class="hint"><kbd>↵</kbd> Open</span>
        <button class="use" disabled={here.id === "root" || busy} onclick={() => useFolder(here)}>
          Use “{here.name}” <kbd>ctrl ↵</kbd>
        </button>
      {/if}
    {/if}
  </footer>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    height: 100%;
    border: 1px solid var(--border);
  }

  svg {
    flex-shrink: 0;
  }

  header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 16px;
    height: 56px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .glass {
    width: 18px;
    height: 18px;
    fill: none;
    stroke: var(--muted);
    stroke-width: 2;
    stroke-linecap: round;
  }

  input {
    flex: 1;
    min-width: 0;
    background: none;
    border: none;
    outline: none;
    color: var(--text);
    font: inherit;
    font-size: 18px;
  }

  input::placeholder {
    color: var(--muted);
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: 200px;
    padding: 3px 10px;
    border-radius: 999px;
    background: var(--tile);
    color: var(--muted);
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .chip svg,
  .row svg {
    width: 14px;
    height: 14px;
    fill: currentColor;
    opacity: 0.8;
  }

  .banner {
    padding: 6px 16px;
    background: var(--accent-soft);
    font-size: 12px;
  }

  .body {
    flex: 1;
    min-height: 0;
    display: flex;
  }

  /* ---------- cards (setup, sign in, empty) ---------- */

  .card,
  .empty {
    margin: auto;
    max-width: 520px;
    padding: 24px;
    color: var(--muted);
  }

  .card.center {
    text-align: center;
    max-width: 440px;
  }

  .card h2 {
    margin: 0 0 8px;
    font-size: 17px;
    font-weight: 600;
    color: var(--text);
  }

  .card p {
    margin: 0 0 14px;
  }

  .card b {
    color: var(--text);
    font-weight: 600;
  }

  .card .heads-up {
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--tile);
    font-size: 12px;
  }

  .card button,
  .use {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 8px 14px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-raised);
    color: var(--text);
    cursor: pointer;
  }

  .card button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }

  button:disabled {
    opacity: 0.55;
    cursor: default;
  }

  /* ---------- folder picker ---------- */

  .picker {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .trail {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 2px;
    padding: 8px 12px 4px;
    font-size: 12px;
  }

  .crumb {
    background: none;
    border: none;
    padding: 2px 4px;
    border-radius: 4px;
    color: var(--muted);
    cursor: pointer;
  }

  .crumb:disabled {
    color: var(--text);
    opacity: 1;
    font-weight: 600;
  }

  .sep {
    color: var(--muted);
  }

  .list {
    flex: 1;
    overflow-y: auto;
    padding: 4px 8px 8px;
  }

  .pad {
    padding: 12px;
    margin: 0;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 8px 10px;
    border: none;
    border-radius: 8px;
    background: none;
    color: var(--text);
    text-align: left;
    cursor: default;
  }

  .row span:nth-child(2) {
    flex: 1;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .row.create {
    color: var(--accent);
    margin-bottom: 4px;
  }

  .plus {
    display: grid;
    place-items: center;
    width: 14px;
    font-size: 16px;
    font-weight: 600;
    line-height: 1;
  }

  .row .go {
    color: var(--muted);
  }

  .row.selected {
    background: var(--accent-soft);
  }

  /* ---------- meme grid ---------- */

  .grid {
    flex: 1;
    overflow-y: auto;
    padding: 10px;
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    grid-auto-rows: min-content;
    gap: 8px;
  }

  .tile {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 4px;
    min-width: 0;
    background: none;
    border: 2px solid transparent;
    border-radius: 10px;
    color: inherit;
    text-align: left;
    cursor: default;
  }

  .tile.selected {
    border-color: var(--accent);
    background: var(--accent-soft);
  }

  .tile.doomed {
    border-color: var(--danger);
  }

  .thumb {
    aspect-ratio: 1;
    border-radius: 6px;
    background: var(--tile);
    overflow: hidden;
    display: grid;
    place-items: center;
  }

  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .name {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    padding: 0 2px;
  }

  .tile.selected .name {
    color: var(--text);
  }

  /* ---------- footer ---------- */

  footer {
    display: flex;
    align-items: center;
    gap: 12px;
    height: 38px;
    padding: 0 14px;
    border-top: 1px solid var(--border);
    background: var(--bg-raised);
    font-size: 12px;
    flex-shrink: 0;
  }

  .current,
  .error,
  .danger-text,
  footer .muted:first-child {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }

  .error,
  .danger-text {
    color: var(--danger);
  }

  .spacer {
    flex: 1;
  }

  .muted,
  .hint,
  .sync {
    color: var(--muted);
    white-space: nowrap;
  }

  .sync.err {
    color: var(--danger);
  }

  .sync.spin {
    color: var(--text);
  }

  .stats {
    font-variant-numeric: tabular-nums;
  }

  .use {
    padding: 3px 10px;
    font-size: 12px;
    border-color: var(--accent);
    background: var(--accent);
    color: #fff;
    white-space: nowrap;
  }

  .use:disabled {
    background: var(--bg-raised);
    border-color: var(--border);
    color: var(--muted);
  }

  kbd {
    font: inherit;
    font-size: 11px;
    padding: 1px 5px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--tile);
    color: var(--text);
  }

  .primary kbd,
  .use:not(:disabled) kbd {
    background: rgba(255, 255, 255, 0.2);
    border-color: transparent;
    color: #fff;
  }
</style>
