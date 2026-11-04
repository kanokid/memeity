<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, tick } from "svelte";
  import Setup from "./Setup.svelte";

  type Hit = { path: string; name: string; rel: string; width: number | null; height: number | null };
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

  /** GIF-picker style: a few big columns of images, no captions. */
  const COLS = 3;
  const LIMIT = 200;
  /** Tile aspect (height / width) is clamped so panoramas and tall screenshots stay usable. */
  const MIN_RATIO = 0.5;
  const MAX_RATIO = 1.6;
  /** Gap between tiles, as a fraction of column width (14px gap / ~220px column). */
  const GAP_UNITS = 0.064;

  let status = $state<Status | null>(null);
  let query = $state("");
  let error = $state<string | null>(null);
  let busy = $state(false);
  let now = $state(Date.now());

  // ready stage
  let hits = $state<Hit[]>([]);
  let selected = $state(0);
  let confirmDelete = $state<Hit | null>(null);
  let searchSeq = 0;

  /** Actions menu (right-click on a tile, or Ctrl+K), positioned in window coordinates. */
  let menu = $state<{ x: number; y: number; index: number } | null>(null);
  let menuSel = $state(0);

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

  // ---------- masonry layout ----------

  function ratioOf(hit: Hit) {
    const r = hit.width && hit.height ? hit.height / hit.width : 1;
    return Math.min(MAX_RATIO, Math.max(MIN_RATIO, r));
  }

  /**
   * Places each hit (in rank order) into the currently shortest column, like a GIF picker.
   * Positions are in "column widths", which is all keyboard navigation needs.
   */
  const masonry = $derived.by(() => {
    const cols: number[][] = Array.from({ length: COLS }, () => []);
    const heights = new Array<number>(COLS).fill(0);
    const col: number[] = [];
    const row: number[] = [];
    const mid: number[] = []; // vertical centre of each tile
    hits.forEach((hit, i) => {
      const c = heights.indexOf(Math.min(...heights));
      const r = ratioOf(hit);
      col[i] = c;
      row[i] = cols[c].length;
      mid[i] = heights[c] + r / 2;
      cols[c].push(i);
      heights[c] += r + GAP_UNITS;
    });
    return { cols, col, row, mid };
  });

  /** The tile in column `c` whose centre is closest to tile `i`'s centre. */
  function nearestIn(c: number, i: number) {
    const candidates = masonry.cols[c];
    if (!candidates?.length) return i;
    return candidates.reduce((best, j) =>
      Math.abs(masonry.mid[j] - masonry.mid[i]) < Math.abs(masonry.mid[best] - masonry.mid[i]) ? j : best,
    );
  }

  function move(dir: "up" | "down" | "left" | "right", steps = 1) {
    if (hits.length === 0) return;
    const i = selected;
    const c = masonry.col[i];
    const r = masonry.row[i];
    if (dir === "up" || dir === "down") {
      const column = masonry.cols[c];
      const next = dir === "down" ? Math.min(column.length - 1, r + steps) : Math.max(0, r - steps);
      select(column[next]);
    } else {
      const nc = dir === "right" ? c + 1 : c - 1;
      if (nc >= 0 && nc < COLS) select(nearestIn(nc, i));
    }
  }

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
    selected = resetSelection ? 0 : Math.min(selected, Math.max(hits.length - 1, 0));
    if (resetSelection) grid?.scrollTo({ top: 0 });
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

  function onInput() {
    error = null;
    confirmDelete = null;
    menu = null;
    if (stage === "ready") runSearch(true);
    else folderSel = 0;
  }

  // ---------- actions menu ----------

  const MENU_W = 230;
  const MENU_H = 92;
  const actions = [
    { label: "Copy to Clipboard", keys: "↵", danger: false, run: (i: number) => copy(i) },
    {
      label: "Delete Meme…",
      keys: "Ctrl ⌫",
      danger: true,
      run: (i: number) => {
        confirmDelete = hits[i];
      },
    },
  ];

  function openMenuAt(e: MouseEvent, i: number) {
    e.preventDefault();
    select(i);
    menuSel = 0;
    menu = {
      // Keep the menu inside the window.
      x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
      y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
      index: i,
    };
  }

  /** Ctrl+K: Raycast-style actions panel, anchored above the action bar. */
  function openActions() {
    if (!current) return;
    menuSel = 0;
    menu = { x: window.innerWidth - MENU_W - 10, y: window.innerHeight - MENU_H - 52, index: selected };
  }

  function runAction(a: number) {
    if (!menu) return;
    const i = menu.index;
    menu = null;
    actions[a].run(i);
  }

  // Suppress the browser's own menu (Back/Refresh/Inspect) everywhere except text fields,
  // where cut/copy/paste is still useful.
  function onContextMenu(e: MouseEvent) {
    if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
  }

  // ---------- keyboard ----------

  function onKeydown(e: KeyboardEvent) {
    const key = e.key;
    const ctrl = e.ctrlKey || e.metaKey;

    if (menu) {
      if (key === "ArrowDown" || key === "ArrowUp") {
        e.preventDefault();
        menuSel = (menuSel + (key === "ArrowDown" ? 1 : actions.length - 1)) % actions.length;
        return;
      }
      if (key === "Enter") {
        e.preventDefault();
        runAction(menuSel);
        return;
      }
      // Anything else dismisses the menu; Esc (and Ctrl+K again) stop there.
      menu = null;
      if (key === "Escape" || (ctrl && key.toLowerCase() === "k")) {
        e.preventDefault();
        return;
      }
    }

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
      if (ctrl && key.toLowerCase() === "k") { e.preventDefault(); openActions(); return; }
      switch (key) {
        case "ArrowRight": e.preventDefault(); move("right"); break;
        case "ArrowLeft":  e.preventDefault(); move("left"); break;
        case "ArrowDown":  e.preventDefault(); move("down"); break;
        case "ArrowUp":    e.preventDefault(); move("up"); break;
        case "PageDown":   e.preventDefault(); move("down", 3); break;
        case "PageUp":     e.preventDefault(); move("up", 3); break;
        case "Enter":      e.preventDefault(); copy(); break;
        case "Backspace":
          // Ctrl+Backspace deletes the selected meme (after confirming). This replaces the
          // search box's delete-a-word shortcut; plain Backspace still edits the search.
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
        menu = null;
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

  function ago(ms: number) {
    const s = Math.max(0, Math.round((now - ms) / 1000));
    if (s < 10) return "just now";
    if (s < 60) return `${s}s ago`;
    const m = Math.round(s / 60);
    return m < 60 ? `${m}m ago` : `${Math.round(m / 60)}h ago`;
  }

  function syncLabel(s: SyncStatus) {
    if (s.state === "error") return "Sync problem";
    if (s.state === "syncing") return "Syncing…";
    if (s.state === "off" || !s.last_synced) return "Not synced yet";
    const { downloaded, uploaded, removed } = s.last;
    const moved = [downloaded && `↓${downloaded}`, uploaded && `↑${uploaded}`, removed && `−${removed}`]
      .filter(Boolean)
      .join(" ");
    return `Synced ${ago(s.last_synced)}${moved ? ` · ${moved}` : ""}`;
  }
</script>

<svelte:window
  onkeydown={onKeydown}
  oncontextmenu={onContextMenu}
  onmousedown={(e) => {
    if (menu && !(e.target instanceof Element && e.target.closest(".menu"))) menu = null;
  }}
  onblur={() => (menu = null)}
/>

{#snippet folderIcon()}
  <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
    <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
  </svg>
{/snippet}

<main class:pane={stage === "ready"}>
  {#if stage === "folder" || stage === "ready"}
    <header>
      <input
        bind:this={input}
        bind:value={query}
        oninput={onInput}
        placeholder={stage === "folder"
          ? `Search folders in ${here.name}…`
          : status?.count
            ? `Search ${status.count} memes…`
            : "Search memes…"}
        spellcheck="false"
        autocomplete="off"
      />
      {#if stage === "ready" && status?.drive_folder}
        <span class="badge" title={status.local_folder ?? ""}>
          {@render folderIcon()}
          {status.drive_folder.name}
        </span>
      {/if}
    </header>
  {/if}

  {#if status?.shortcut_error}
    <div class="banner">{status.shortcut_error}</div>
  {/if}
  {#if error}
    <!-- Full error text (not truncated) and selectable, so it can be copied into a bug report. -->
    <div class="banner error-banner" role="alert">
      <span class="error-text">{error}</span>
      <button class="dismiss" aria-label="Dismiss error" onclick={() => (error = null)}>
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12" /></svg>
      </button>
    </div>
  {/if}

  <section class="body">
    {#if !status}
      <div></div>
    {:else if stage === "setup"}
      <Setup onLoad={loadClientFile} {busy} />
    {:else if stage === "signin"}
      <div class="empty">
        <div class="empty-icon">
          <svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="11" width="18" height="11" rx="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" /></svg>
        </div>
        <h2>Sign in to Google Drive</h2>
        <p>memeity syncs one Drive folder to this device. Your browser opens to sign in, and the token stays in Windows Credential Manager.</p>
        <p class="callout">
          Google will say <b>“Google hasn't verified this app”</b>. It's your own project, so click <b>Continue</b>, then allow Drive access.
        </p>
        <button class="btn primary" onclick={signIn} disabled={busy}>
          {busy ? "Waiting for your browser…" : "Sign in with Google"}
          {#if !busy}<kbd>↵</kbd>{/if}
        </button>
      </div>
    {:else if stage === "folder"}
      <div class="picker">
        <nav class="trail">
          {#each trail as crumb, i (crumb.id)}
            {#if i > 0}<span class="sep">/</span>{/if}
            <button class="crumb" disabled={i === trail.length - 1} onclick={() => goUp(i)}>{crumb.name}</button>
          {/each}
        </nav>
        <div class="list" bind:this={list}>
          {#if foldersLoading}
            {#each [0, 1, 2, 3] as n (n)}<div class="row skeleton"></div>{/each}
          {:else}
            {#if showCreate}
              <p class="group">Quick start</p>
              <button
                class="row"
                class:selected={folderSel === 0}
                data-i={0}
                tabindex="-1"
                disabled={busy}
                onmousedown={(e) => e.preventDefault()}
                onclick={() => (folderSel = 0)}
                ondblclick={createMemeityFolder}
              >
                <svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14" /></svg>
                <span class="row-label">Create “{NEW_FOLDER}” folder in {here.name}</span>
                <span class="row-hint">Create &amp; use</span>
              </button>
            {/if}
            <p class="group">Folders in {here.name}</p>
            {#if shownFolders.length === 0}
              <p class="none">{query.trim() ? "No folders match." : "No sub-folders here."}</p>
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
                {@render folderIcon()}
                <span class="row-label">{f.name}</span>
                <span class="row-hint">Open</span>
              </button>
            {/each}
          {/if}
        </div>
      </div>
    {:else if stage === "ready"}
      {#if status.count === 0}
        <div class="empty">
          <div class="empty-icon">{@render folderIcon()}</div>
          {#if status.sync.state === "syncing"}
            <h2>Syncing from Google Drive…</h2>
            <p>Your memes will appear here as they download.</p>
          {:else}
            <h2>No memes in “{status.drive_folder?.name}” yet</h2>
            <p>Add images to the folder on Google Drive, or drop them into the local folder and they'll upload.</p>
            <button class="btn outline" onclick={() => invoke("open_folder")}>Open local folder</button>
          {/if}
        </div>
      {:else if hits.length === 0}
        <div class="empty">
          <h2>No results</h2>
          <p>Nothing matches “{query}”.</p>
        </div>
      {:else}
        <div class="masonry" bind:this={grid} onscroll={() => (menu = null)}>
          {#each masonry.cols as column, c (c)}
            <div class="col">
              {#each column as i (hits[i].path)}
                {@const hit = hits[i]}
                <button
                  class="tile"
                  class:selected={i === selected}
                  class:doomed={confirmDelete?.path === hit.path}
                  data-i={i}
                  tabindex="-1"
                  style:aspect-ratio="1 / {ratioOf(hit)}"
                  title={hit.name}
                  onmousedown={(e) => e.preventDefault()}
                  onclick={() => {
                    // GIF-picker style: one click acts, same as Enter.
                    select(i);
                    copy(i);
                  }}
                  oncontextmenu={(e) => openMenuAt(e, i)}
                >
                  <img src={convertFileSrc(hit.path)} alt={hit.name} loading="lazy" decoding="async" draggable="false" />
                </button>
              {/each}
            </div>
          {/each}
        </div>
      {/if}
    {/if}
  </section>

  <footer>
    {#if confirmDelete}
      <span class="danger-text">Delete “{confirmDelete.name}” from Drive and every device?</span>
      <span class="spacer"></span>
      <button class="btn ghost sm" onclick={() => (confirmDelete = null)}>Cancel <kbd>Esc</kbd></button>
      <button class="btn destructive sm" onclick={() => confirmDelete && doDelete(confirmDelete)} disabled={busy}>
        Delete <kbd>↵</kbd>
      </button>
    {:else}
      {#if stage === "ready" && status}
        <span
          class="status"
          class:err={status.sync.state === "error"}
          class:busy={status.sync.state === "syncing"}
          title={status.sync.message ?? status.account ?? ""}
        >
          <i class="dot"></i>{syncLabel(status.sync)}
        </span>
      {:else if stage === "folder"}
        <span class="status muted">Choose the Drive folder that holds your memes</span>
      {:else if status?.account}
        <span class="status muted">{status.account}</span>
      {:else}
        <span class="status muted">memeity</span>
      {/if}
      <span class="spacer"></span>

      {#if stage === "ready" && current}
        <button class="action primary-action" onclick={() => copy()}>
          <svg class="icon action-icon" viewBox="0 0 24 24" aria-hidden="true">
            <rect x="8" y="2" width="8" height="4" rx="1" />
            <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
          </svg>
          Copy to Clipboard <kbd>↵</kbd>
        </button>
        <span class="divider"></span>
        <button class="action" onclick={openActions}>Actions <kbd>Ctrl</kbd><kbd>K</kbd></button>
      {:else if stage === "folder"}
        {#if trail.length > 1}<span class="action static">Back <kbd>⌫</kbd></span>{/if}
        <span class="action static">Open <kbd>↵</kbd></span>
        <span class="divider"></span>
        <button class="btn primary sm" disabled={here.id === "root" || busy} onclick={() => useFolder(here)}>
          Use “{here.name}” <kbd>Ctrl</kbd><kbd>↵</kbd>
        </button>
      {/if}
    {/if}
  </footer>
</main>

{#if menu}
  <div class="menu" role="menu" style:left="{menu.x}px" style:top="{menu.y}px" style:width="{MENU_W}px">
    {#each actions as a, n (a.label)}
      {#if n > 0 && a.danger}<div class="menu-sep"></div>{/if}
      <button
        role="menuitem"
        class:danger={a.danger}
        class:active={n === menuSel}
        onmouseenter={() => (menuSel = n)}
        onclick={() => runAction(n)}
      >
        <span>{a.label}</span><span class="keys">{a.keys}</span>
      </button>
    {/each}
  </div>
{/if}

<style>
  main {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  /* The frameless pane draws its own hairline edge; Windows rounds the corners. */
  main.pane {
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }

  .icon {
    width: 16px;
    height: 16px;
    flex-shrink: 0;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  /* ---------- search bar ---------- */

  header {
    display: flex;
    align-items: center;
    gap: 12px;
    height: 56px;
    padding: 0 18px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  input {
    flex: 1;
    min-width: 0;
    background: none;
    border: none;
    outline: none;
    color: var(--fg);
    font: inherit;
    font-size: 17px;
    letter-spacing: -0.01em;
  }

  input::placeholder {
    color: var(--muted-fg);
  }

  .badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: 200px;
    height: 24px;
    padding: 0 8px;
    border: 1px solid var(--border);
    border-radius: 6px;
    color: var(--muted-fg);
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .badge .icon {
    width: 13px;
    height: 13px;
  }

  .banner {
    padding: 8px 18px;
    border-bottom: 1px solid var(--border);
    background: var(--muted);
    color: var(--fg);
    font-size: 12px;
  }

  .error-banner {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding-right: 10px;
    border-bottom-color: color-mix(in srgb, var(--destructive) 40%, var(--border));
    background: color-mix(in srgb, var(--destructive) 12%, var(--bg));
  }

  .error-text {
    flex: 1;
    min-width: 0;
    max-height: 72px;
    overflow-y: auto;
    color: var(--destructive);
    word-break: break-word;
    user-select: text;
    -webkit-user-select: text;
    cursor: text;
  }

  .dismiss {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    flex-shrink: 0;
    padding: 0;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--muted-fg);
    cursor: pointer;
  }

  .dismiss:hover {
    background: var(--accent);
    color: var(--fg);
  }

  .dismiss .icon {
    width: 14px;
    height: 14px;
  }

  .body {
    flex: 1;
    min-height: 0;
    display: flex;
  }

  /* ---------- masonry ---------- */

  .masonry {
    flex: 1;
    display: flex;
    align-items: flex-start;
    /* Rings extend 4px past each tile, so 14px leaves clear space between neighbours. */
    gap: 14px;
    /* Breathing room between the grid and the pane's edges. */
    padding: 20px 22px;
    overflow-y: auto;
  }

  .col {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .tile {
    position: relative;
    display: block;
    width: 100%;
    padding: 0;
    border: none;
    border-radius: 8px;
    overflow: hidden;
    background: var(--muted);
    cursor: pointer;
    outline: 2px solid transparent;
    outline-offset: 2px;
    transition: outline-color 100ms ease;
  }

  .tile img {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
    transition: filter 160ms ease;
  }

  /* Hover: a softer version of the selection ring, and a brightness lift. */
  .tile:hover {
    outline-color: color-mix(in srgb, var(--ring) 45%, transparent);
  }

  .tile:hover img {
    filter: brightness(1.15) saturate(1.1);
  }

  .tile.selected {
    outline-color: var(--ring);
  }

  .tile.doomed {
    outline-color: var(--destructive);
  }

  /* ---------- empty states ---------- */

  .empty {
    margin: auto;
    max-width: 400px;
    padding: 24px;
    text-align: center;
    color: var(--muted-fg);
  }

  .empty-icon {
    display: inline-grid;
    place-items: center;
    width: 40px;
    height: 40px;
    margin-bottom: 14px;
    border: 1px solid var(--border);
    border-radius: 10px;
    color: var(--fg);
  }

  .empty h2 {
    margin: 0 0 6px;
    font-size: 15px;
    font-weight: 600;
    letter-spacing: -0.01em;
    color: var(--fg);
  }

  .empty p {
    margin: 0 0 16px;
    font-size: 13px;
  }

  .callout {
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    text-align: left;
    font-size: 12px !important;
  }

  .empty b,
  .callout b {
    color: var(--fg);
    font-weight: 500;
  }

  /* ---------- folder picker (cmdk-style list) ---------- */

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
    padding: 10px 14px 0;
    font-size: 12px;
  }

  .crumb {
    padding: 2px 4px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--muted-fg);
    cursor: pointer;
  }

  .crumb:hover:not(:disabled) {
    color: var(--fg);
  }

  .crumb:disabled {
    color: var(--fg);
    font-weight: 500;
    cursor: default;
  }

  .sep {
    color: var(--border-strong);
  }

  .list {
    flex: 1;
    overflow-y: auto;
    padding: 4px 8px 8px;
  }

  .group {
    margin: 0;
    padding: 10px 8px 6px;
    font-size: 11px;
    font-weight: 500;
    color: var(--muted-fg);
  }

  .none {
    margin: 0;
    padding: 6px 8px;
    font-size: 13px;
    color: var(--muted-fg);
  }

  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    height: 38px;
    padding: 0 10px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--fg);
    font-size: 13px;
    text-align: left;
    cursor: default;
  }

  .row .icon {
    color: var(--muted-fg);
  }

  .row-label {
    flex: 1;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .row-hint {
    color: var(--muted-fg);
    font-size: 12px;
    opacity: 0;
  }

  .row.selected {
    background: var(--accent);
  }

  .row.selected .row-hint {
    opacity: 1;
  }

  .row.skeleton {
    margin-bottom: 4px;
    background: var(--muted);
    animation: pulse 1.4s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.5;
    }
  }

  /* ---------- action bar ---------- */

  footer {
    display: flex;
    align-items: center;
    gap: 6px;
    height: 44px;
    padding: 0 8px 0 16px;
    border-top: 1px solid var(--border);
    background: var(--bar);
    font-size: 12px;
    flex-shrink: 0;
  }

  .spacer {
    flex: 1;
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    color: var(--muted-fg);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .dot {
    width: 7px;
    height: 7px;
    flex-shrink: 0;
    border-radius: 50%;
    background: var(--success);
  }

  .status.busy .dot {
    background: var(--warning);
    animation: pulse 1s ease-in-out infinite;
  }

  .status.err {
    color: var(--destructive);
  }

  .status.err .dot {
    background: var(--destructive);
  }

  .danger-text {
    color: var(--fg);
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }

  .action {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 8px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    cursor: pointer;
  }

  .action:hover:not(.static) {
    background: var(--accent);
    color: var(--fg);
  }

  .action.static {
    cursor: default;
  }

  .primary-action {
    color: var(--fg);
  }

  .action-icon {
    width: 14px;
    height: 14px;
  }

  .divider {
    width: 1px;
    height: 16px;
    background: var(--border);
  }

  /* ---------- buttons (shadcn) ---------- */

  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    height: 36px;
    padding: 0 16px;
    border: 1px solid transparent;
    border-radius: 6px;
    font-size: 13px;
    font-weight: 500;
    white-space: nowrap;
    cursor: pointer;
    transition: background 120ms ease, opacity 120ms ease;
  }

  .btn.sm {
    height: 28px;
    padding: 0 10px;
    font-size: 12px;
  }

  .btn.primary {
    background: var(--primary);
    color: var(--primary-fg);
  }

  .btn.primary:hover:not(:disabled) {
    opacity: 0.9;
  }

  .btn.outline {
    border-color: var(--border);
    background: var(--bg);
    color: var(--fg);
  }

  .btn.outline:hover {
    background: var(--accent);
  }

  .btn.ghost {
    background: none;
    color: var(--muted-fg);
  }

  .btn.ghost:hover {
    background: var(--accent);
    color: var(--fg);
  }

  .btn.destructive {
    background: var(--destructive);
    color: #fff;
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn.primary kbd,
  .btn.destructive kbd {
    border-color: transparent;
    background: color-mix(in srgb, currentColor 15%, transparent);
    color: inherit;
  }

  /* ---------- actions menu ---------- */

  .menu {
    position: fixed;
    z-index: 10;
    padding: 4px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--popover);
    box-shadow:
      0 10px 38px -10px rgba(0, 0, 0, 0.5),
      0 10px 20px -15px rgba(0, 0, 0, 0.4);
  }

  .menu button {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    height: 32px;
    padding: 0 8px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--fg);
    font-size: 13px;
    text-align: left;
    cursor: default;
  }

  .menu button.active {
    background: var(--accent);
  }

  .menu button.danger {
    color: var(--destructive);
  }

  .keys {
    color: var(--muted-fg);
    font-size: 11px;
    letter-spacing: 0.02em;
  }

  .menu-sep {
    height: 1px;
    margin: 4px -4px;
    background: var(--border);
  }
</style>
