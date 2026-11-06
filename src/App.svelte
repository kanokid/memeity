<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, tick } from "svelte";
  import Setup from "./Setup.svelte";
  import logo from "./assets/logo.png";

  type Hit = {
    path: string;
    name: string;
    rel: string;
    width: number | null;
    height: number | null;
    /** GIF / animated WebP: shown as a still thumbnail, animated on hover or selection. */
    animated: boolean;
    /** Changes when the file does, so its cached thumbnail refreshes. */
    version: number;
    /** Pinned to the top of the pane (synced to every device). */
    pinned: boolean;
  };
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
    ocr: OcrStatus;
    client_file: string | null;
    /** What Enter / click does; Shift does the other. Set in Settings > General. */
    primary_action: "paste" | "copy";
  };
  type OcrStatus = {
    state: "idle" | "preparing" | "reading" | "unavailable";
    done: number;
    total: number;
    message: string | null;
  };

  /** GIF-picker style: a few big columns of images, no captions. */
  const COLS = 3;
  /** Results per page; more load as you scroll (infinite scroll). */
  const PAGE = 90;
  /** Start loading the next page this many pixels before the bottom. */
  const LOAD_AHEAD_PX = 700;
  /** Tile aspect (height / width) is clamped so panoramas and tall screenshots stay usable. */
  const MIN_RATIO = 0.35;
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
  let hovered = $state(-1);
  /** The OS asks for less motion: GIFs only play when selected, not on hover. */
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const stage = $derived(status?.stage);
  const current = $derived(hits[selected]);

  /**
   * Small cached thumbnail (see thumbs.rs), or the original for animated memes in play
   * (`active` = selected or hovered). Pure, so a tile's src only re-renders when its own
   * `active` flips, not on every arrow key / hover elsewhere in the grid.
   */
  function tileSrc(hit: Hit, active: boolean) {
    if (hit.animated && active) return `${convertFileSrc(hit.path, "thumb")}?full=1&v=${hit.version}`;
    return `${convertFileSrc(hit.path, "thumb")}?v=${hit.version}`;
  }
  let confirmDelete = $state<Hit | null>(null);

  // ---------- quick editor ----------

  /** The meme open in the editor (the pane grows to fit it), if any. */
  let editing = $state<Hit | null>(null);
  /** Loaded the first time it's used, so the search pane never waits for it. */
  let Editor = $state<typeof import("./editor/Editor.svelte").default | null>(null);

  async function openEditor(i = selected) {
    const hit = hits[i];
    if (!hit || editing) return;
    menu = null;
    confirmDelete = null;
    try {
      Editor ??= (await import("./editor/Editor.svelte")).default;
      await invoke("set_editing", { active: true });
      editing = hit;
    } catch (e) {
      error = String(e);
    }
  }

  async function closeEditor(saved?: { rel: string; duplicate: boolean }) {
    editing = null;
    await invoke("set_editing", { active: false });
    input?.focus();
    if (saved) {
      await showMeme(saved.rel);
      const name = saved.rel.split("/").pop();
      showToast({ message: saved.duplicate ? `${name} is already in your library` : `Saved as ${name}` }, 4000);
    }
  }
  let searchSeq = 0;

  /**
   * Actions menu (right-click on a tile, or Ctrl+K), positioned in window coordinates.
   * `bottom`, if set, anchors its bottom edge there instead (resolved once measured).
   */
  let menu = $state<{ x: number; y: number; bottom?: number; index: number } | null>(null);
  let menuSel = $state(0);

  /** Details view (Ctrl+I): big preview + the text OCR read from the selected meme. */
  type MemeText = {
    state: "read" | "pending" | "failed";
    text: string | null;
    lang: string | null;
    at: string | null;
    /** The user typed/corrected this text; OCR won't overwrite it. */
    edited: boolean;
  };
  let showDetails = $state(false);
  /** The last text loaded, tagged with the meme it belongs to. */
  let loaded = $state<{ path: string; text: MemeText } | null>(null);
  /** Text for the *current* meme only: never a previous selection's text while the new one loads. */
  const details = $derived(loaded && loaded.path === current?.path ? loaded.text : null);
  let copiedText = $state(false);
  let detailsSeq = 0;

  // Follow the selection while the details view is open, and re-check while OCR is
  // still working through the queue (so "Not read yet" updates by itself).
  $effect(() => {
    const path = current?.path;
    const ocrProgress = status?.ocr.done;
    if ((!showDetails && view !== "list") || !path) return;
    void ocrProgress;
    const seq = ++detailsSeq;
    invoke<MemeText>("meme_text", { path })
      .then((t) => {
        if (seq === detailsSeq) loaded = { path, text: t };
      })
      .catch((e) => {
        if (seq === detailsSeq) error = String(e);
      });
  });

  function toggleDetails(open = !showDetails) {
    if (open && !current) return;
    showDetails = open;
    loaded = null;
    copiedText = false;
    renaming = false;
    editingText = false;
  }

  // ---------- editing (rename, retype text) ----------

  let renaming = $state(false);
  let nameDraft = $state("");
  let editingText = $state(false);
  let textDraft = $state("");
  let nameInput: HTMLInputElement | undefined = $state();
  let textInput: HTMLTextAreaElement | undefined = $state();

  const stemOf = (name: string) => (name.lastIndexOf(".") > 0 ? name.slice(0, name.lastIndexOf(".")) : name);
  const extOf = (name: string) => (name.lastIndexOf(".") > 0 ? name.slice(name.lastIndexOf(".") + 1) : "");

  /** Rename (F2): opens the details view with the name field ready to type. */
  async function startRename() {
    if (!current) return;
    if (!showDetails) toggleDetails(true);
    editingText = false;
    nameDraft = stemOf(current.name);
    renaming = true;
    await tick();
    nameInput?.focus();
    nameInput?.select();
  }

  function stopEditing() {
    renaming = false;
    editingText = false;
    input?.focus(); // back to the search box, so the arrow keys work again
  }

  const saveRename = () =>
    act(async () => {
      if (!current) return;
      const path = current.path;
      const oldStem = stemOf(current.name);
      const renamed = await invoke<string>("rename_meme", { path, name: nameDraft });
      stopEditing();
      // Keep the renamed meme selected (its rank, and so its index, may have changed).
      wantSelected(renamed);
      await refresh();
      if (renamed !== path) {
        const undo = () => undoRename(renamed, oldStem);
        offerUndo(undo, UNDO_MS);
        showToast({ message: `Renamed to ${baseName(renamed)}`, action: { label: "Undo", run: undo } }, DELETED_TOAST_MS);
      }
    });

  /** Puts a rename back (the extension never changed, so the old stem is enough). */
  const undoRename = (path: string, oldStem: string) =>
    act(async () => {
      lastUndo = null;
      toast = null;
      const restored = await invoke<string>("rename_meme", { path, name: oldStem });
      wantSelected(restored);
      await refresh();
      showToast({ message: `Renamed back to ${baseName(restored)}` }, 2000);
    });

  const baseName = (path: string) => path.split(/[\\/]/).pop() ?? path;

  /**
   * Opens the text editor pre-filled with the meme's current text. Waits for that text to
   * load first, so the editor never opens empty (saving that would wipe the real text).
   */
  async function startEditText() {
    const hit = current;
    if (!hit) return;
    if (!showDetails) toggleDetails(true);
    renaming = false;
    let text = details;
    if (!text) {
      try {
        text = await invoke<MemeText>("meme_text", { path: hit.path });
      } catch (e) {
        error = String(e);
        return;
      }
      // The selection moved on (or details closed) while loading: don't edit the wrong meme.
      if (current?.path !== hit.path || !showDetails) return;
      loaded = { path: hit.path, text };
    }
    textDraft = text.text ?? "";
    editingText = true;
    await tick();
    textInput?.focus();
  }

  const saveText = () =>
    act(async () => {
      if (!editingText || !current) return;
      const path = current.path; // fixed before any await: the selection may change meanwhile
      await invoke("set_meme_text", { path, text: textDraft });
      stopEditing();
      loaded = { path, text: await invoke<MemeText>("meme_text", { path }) };
      await runSearch(false); // the new text is searchable right away
    });

  /** Enter/Esc inside the edit fields (they handle their own keys). */
  function onEditKey(e: KeyboardEvent, save: () => void, multiline: boolean) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      stopEditing();
    } else if (e.key === "Enter" && (!multiline || e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      e.stopPropagation();
      save();
    }
  }

  async function copyText() {
    if (!details?.text) return;
    await navigator.clipboard.writeText(details.text);
    copiedText = true;
    setTimeout(() => (copiedText = false), 1500);
  }

  function formatWhen(at: string | null) {
    if (!at) return "";
    const d = new Date(at);
    return isNaN(d.getTime()) ? "" : d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
  }

  let input: HTMLInputElement | undefined = $state();
  let grid: HTMLDivElement | undefined = $state();

  /** Platform-specific wording for shortcut hints (the handlers accept Ctrl or Cmd). */
  const isMac = navigator.userAgent.includes("Mac");
  const MOD = isMac ? "⌘" : "Ctrl";
  const credentialStore = isMac
    ? "your macOS Keychain"
    : navigator.userAgent.includes("Windows")
      ? "Windows Credential Manager"
      : "your system keyring";


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
    const ratio: number[] = []; // height / width of each tile
    const top: number[] = []; // sum of the ratios of the tiles above it in its column
    const sums = new Array<number>(COLS).fill(0);
    hits.forEach((hit, i) => {
      const c = heights.indexOf(Math.min(...heights));
      const r = ratioOf(hit);
      col[i] = c;
      row[i] = cols[c].length;
      mid[i] = heights[c] + r / 2;
      ratio[i] = r;
      top[i] = sums[c];
      cols[c].push(i);
      heights[c] += r + GAP_UNITS;
      sums[c] += r;
    });
    return { cols, col, row, mid, ratio, top };
  });

  // ---------- windowing ----------
  // Only tiles near the viewport are rendered; each column is padded above and below to
  // its full computed height, so the scrollbar and positions match the full layout.

  /** Must match the .masonry / .col CSS. */
  const GRID_PAD_Y = 20;
  const GRID_PAD_X = 22;
  const GAP_PX = 14;

  let scrollTop = $state(0);
  let viewH = $state(0);
  let colW = $state(0);

  function measureGrid() {
    if (!grid) return;
    viewH = grid.clientHeight;
    scrollTop = grid.scrollTop;
    colW = Math.max(1, (grid.clientWidth - 2 * GRID_PAD_X - (COLS - 1) * GAP_PX) / COLS);
  }

  $effect(() => {
    const el = grid;
    if (!el) return;
    measureGrid();
    const ro = new ResizeObserver(measureGrid);
    ro.observe(el);
    return () => ro.disconnect();
  });

  /** A tile's top, relative to the start of its column, in px. */
  const tileTopPx = (i: number) => masonry.top[i] * colW + masonry.row[i] * GAP_PX;
  const tileHeightPx = (i: number) => masonry.ratio[i] * colW;

  /** Per column: the rendered slice plus the padding standing in for the rest. */
  const windowed = $derived.by(() => {
    // Not measured yet: render the first screenful-ish so there's something to measure.
    const buffer = viewH || 1000;
    const from = scrollTop - GRID_PAD_Y - buffer;
    const to = scrollTop - GRID_PAD_Y + (viewH || 1000) + buffer;
    return masonry.cols.map((column) => {
      if (column.length === 0) return { items: [] as number[], padTop: 0, padBottom: 0 };
      let start = 0;
      while (start < column.length - 1 && tileTopPx(column[start]) + tileHeightPx(column[start]) < from) start++;
      let end = start;
      while (end < column.length - 1 && tileTopPx(column[end + 1]) <= to) end++;
      const lastI = column[column.length - 1];
      const colHeight = tileTopPx(lastI) + tileHeightPx(lastI);
      const endI = column[end];
      return {
        items: column.slice(start, end + 1),
        padTop: tileTopPx(column[start]),
        padBottom: Math.max(0, colHeight - tileTopPx(endI) - tileHeightPx(endI)),
      };
    });
  });

  /** Scrolls the grid just enough to show tile `i` (it may not be rendered yet). */
  function scrollToTile(i: number) {
    if (view === "list") return scrollToRow(i);
    if (!grid || masonry.top[i] === undefined) return;
    measureGrid();
    const margin = 6; // room for the selection ring
    const top = GRID_PAD_Y + tileTopPx(i) - margin;
    const bottom = GRID_PAD_Y + tileTopPx(i) + tileHeightPx(i) + margin;
    if (top < grid.scrollTop) grid.scrollTop = top;
    else if (bottom > grid.scrollTop + grid.clientHeight) grid.scrollTop = bottom - grid.clientHeight;
  }

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
    if (view === "list") {
      if (dir === "up" || dir === "down") select(selected + (dir === "down" ? 1 : -1) * (steps > 1 ? LIST_PAGE : 1));
      return;
    }
    const i = selected;
    const c = masonry.col[i];
    const r = masonry.row[i];
    if (dir === "up" || dir === "down") {
      const column = masonry.cols[c];
      const last = column.length - 1;
      // Already at the top/bottom of this column: scroll the whole grid to that end,
      // so the padding (and anything partly hidden) comes fully into view.
      if ((dir === "up" && r === 0) || (dir === "down" && r === last)) {
        grid?.scrollTo({ top: dir === "up" ? 0 : grid.scrollHeight, behavior: "smooth" });
        return;
      }
      const next = dir === "down" ? Math.min(last, r + steps) : Math.max(0, r - steps);
      select(column[next]).then(() => {
        // Landing on a column's first meme also shows the very top of the grid (every
        // column starts there). The bottom isn't forced: columns end at different heights.
        if (next === 0) grid?.scrollTo({ top: 0, behavior: "smooth" });
      });
    } else {
      const nc = dir === "right" ? c + 1 : c - 1;
      if (nc >= 0 && nc < COLS) select(nearestIn(nc, i));
    }
  }

  // ---------- drag a meme out into another app ----------

  /** Our own drag is under way: the pane mustn't treat it as something dropped in. */
  let draggingOut = $state(false);
  /** When our last drag ended. Its drop on the pane can arrive just after the OS says it's over. */
  let dragOutEndedAt = 0;
  const ownDrag = () => draggingOut || Date.now() - dragOutEndedAt < 1000;
  /** Mouse pressed on a tile/row; becomes a drag once it moves a few pixels. */
  let pressed: { i: number; x: number; y: number } | null = null;
  /** A drag just started from a press: swallow the click that may follow it. */
  let swallowClick = false;
  const DRAG_THRESHOLD = 6;

  function pressItem(e: MouseEvent, i: number) {
    e.preventDefault(); // keep focus (and the arrow keys) in the search box
    swallowClick = false;
    pressed = e.button === 0 ? { i, x: e.clientX, y: e.clientY } : null;
  }

  function onMouseMove(e: MouseEvent) {
    if (!pressed) return;
    if (!(e.buttons & 1)) {
      pressed = null;
      return;
    }
    if (Math.hypot(e.clientX - pressed.x, e.clientY - pressed.y) < DRAG_THRESHOLD) return;
    const hit = hits[pressed.i];
    pressed = null;
    if (!hit) return;
    swallowClick = true;
    draggingOut = true;
    invoke("drag_meme", { path: hit.path }).catch((err) => {
      draggingOut = false;
      error = String(err);
    });
  }

  /** True (once) if this click is the tail end of a drag. */
  function clickWasDrag() {
    const was = swallowClick;
    swallowClick = false;
    return was;
  }

  // ---------- Ctrl/Cmd + 1-9: act on the Nth result ----------

  /** Ctrl/Cmd held a moment: number badges show on the first nine results. */
  let showNumbers = $state(false);
  let numbersTimer: ReturnType<typeof setTimeout> | undefined;
  const isModKey = (key: string) => key === (isMac ? "Meta" : "Control");

  function hideNumbers() {
    clearTimeout(numbersTimer);
    showNumbers = false;
  }

  // ---------- list view (Ctrl+L): results on the left, preview + details on the right ----------

  type View = "grid" | "list";
  let view = $state<View>(readView());
  let list: HTMLDivElement | undefined = $state();
  /** Row height and top/bottom padding of the list, in px (must match `.row` / `.list-rows`). */
  const ROW_H = 60;
  const LIST_PAD = 8;
  /** PageUp / PageDown moves this many rows in the list. */
  const LIST_PAGE = 8;
  let listScroll = $state(0);
  let listH = $state(0);

  function readView(): View {
    try {
      return localStorage.getItem("view") === "list" ? "list" : "grid";
    } catch {
      return "grid";
    }
  }

  async function toggleView() {
    view = view === "grid" ? "list" : "grid";
    menu = null;
    try {
      localStorage.setItem("view", view);
    } catch {
      // private mode etc.: the choice just isn't remembered
    }
    await tick();
    scrollToTile(selected);
    maybeLoadMore();
  }

  function measureList() {
    if (!list) return;
    listH = list.clientHeight;
    listScroll = list.scrollTop;
  }

  $effect(() => {
    const el = list;
    if (!el) return;
    measureList();
    const ro = new ResizeObserver(measureList);
    ro.observe(el);
    return () => ro.disconnect();
  });

  /** Rows near the viewport (the rest are just space in a tall container). */
  const listRows = $derived.by(() => {
    const h = listH || 600;
    const first = Math.max(0, Math.floor((listScroll - LIST_PAD - h) / ROW_H));
    const last = Math.min(hits.length, Math.ceil((listScroll - LIST_PAD + 2 * h) / ROW_H));
    return Array.from({ length: Math.max(0, last - first) }, (_, k) => first + k);
  });

  function scrollToRow(i: number) {
    if (!list) return;
    const top = LIST_PAD + i * ROW_H;
    if (top < list.scrollTop) list.scrollTop = top - LIST_PAD;
    else if (top + ROW_H > list.scrollTop + list.clientHeight) list.scrollTop = top + ROW_H + LIST_PAD - list.clientHeight;
  }

  const fileType = (hit: Hit) => (extOf(hit.name) || "image").toUpperCase();
  const folderOf = (hit: Hit) => (hit.rel.includes("/") ? hit.rel.slice(0, hit.rel.lastIndexOf("/")) : "");
  const sizeLabel = (hit: Hit) => (hit.width && hit.height ? `${hit.width} × ${hit.height}` : "");

  // ---------- shared ----------

  async function refresh(resetSelection = false) {
    status = await invoke<Status>("get_status");
    if (status.stage === "ready") await runSearch(resetSelection);
    // Signed in but no library folder yet (sign-in's own setup failed, e.g. offline): retry once.
    if (status.stage === "folder" && !folderTried) setupFolder();
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

  // ---------- duplicate cleanup prompt ----------

  /** Identical extra copies in the library (e.g. left over from a sync problem). */
  let duplicateCount = $state(0);
  /** The count the user last dismissed the prompt at; it comes back only if it grows. */
  let duplicatesDismissedAt = $state(readDismissed());

  function readDismissed(): number {
    try {
      return Number(localStorage.getItem("duplicatesDismissedAt") ?? 0) || 0;
    } catch {
      return 0;
    }
  }

  async function checkDuplicates() {
    try {
      duplicateCount = await invoke<number>("duplicate_count");
    } catch {
      duplicateCount = 0; // just a hint; never worth an error banner
    }
  }

  function dismissDuplicates() {
    duplicatesDismissedAt = duplicateCount;
    try {
      localStorage.setItem("duplicatesDismissedAt", String(duplicateCount));
    } catch {
      // private mode etc.: it simply comes back next launch
    }
  }

  /** The shortcut was taken at startup: try registering it again. */
  const retryShortcut = () =>
    act(async () => {
      status = await invoke<Status>("retry_shortcut");
    });

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

  // ---------- library folder ----------

  /** Set once we've tried, so a failure shows a Retry button instead of looping. */
  let folderTried = $state(false);

  const setupFolder = () =>
    act(async () => {
      folderTried = true;
      status = await invoke<Status>("setup_drive_folder");
      await runSearch(true);
    });

  // ---------- search grid ----------

  /** Total matches for the current query (hits may hold fewer: pages load as you scroll). */
  let total = $state(0);
  /** The search (searchSeq) whose next page is loading, so an old query's slow page can't block a new one. */
  let loadingMoreSeq = -1;
  /**
   * A meme to select (by path or library-relative path) as soon as the results include
   * it: after a rename, or a meme just added. Whichever search or page load lands next
   * picks it up, so it can't be lost to another refresh running at the same moment
   * (adding a file also refreshes the library). Expires, so it can't grab the selection
   * much later.
   */
  let pendingSelect: { key: string; until: number } | null = null;
  const PENDING_SELECT_MS = 5000;

  function wantSelected(key: string) {
    pendingSelect = { key, until: Date.now() + PENDING_SELECT_MS };
  }

  /** Selects the pending meme if it's in `hits` (returns its index), else loads the next page. */
  function followPending(): number {
    const want = pendingSelect;
    if (!want) return -1;
    if (Date.now() > want.until) {
      pendingSelect = null;
      return -1;
    }
    const i = hits.findIndex((h) => h.path === want.key || h.rel === want.key);
    if (i >= 0) {
      pendingSelect = null;
      selected = i;
    } else if (hits.length < total) {
      loadMore(); // not on the pages loaded so far
    }
    return i;
  }

  async function runSearch(resetSelection: boolean) {
    const seq = ++searchSeq;
    // A refresh (library changed) keeps however many pages were already loaded.
    const limit = resetSelection ? PAGE : Math.max(PAGE, hits.length);
    let res: SearchResult;
    try {
      res = await invoke<SearchResult>("search", { query, offset: 0, limit });
    } catch (e) {
      if (seq === searchSeq) error = `Search failed: ${e}`;
      return;
    }
    if (seq !== searchSeq) return; // a newer keystroke already won
    hits = res.hits;
    total = res.total;
    selected = resetSelection ? 0 : Math.min(selected, Math.max(hits.length - 1, 0));
    if (resetSelection) (view === "list" ? list : grid)?.scrollTo({ top: 0 });
    const found = followPending();
    await tick();
    if (found >= 0) scrollToTile(found);
    maybeLoadMore(); // the first page might not fill the pane
  }

  /** Appends the next page of results. */
  async function loadMore() {
    const seq = searchSeq;
    if (loadingMoreSeq === seq || hits.length >= total) return;
    loadingMoreSeq = seq;
    try {
      const res = await invoke<SearchResult>("search", { query, offset: hits.length, limit: PAGE });
      if (seq !== searchSeq) return; // the query changed meanwhile
      hits = [...hits, ...res.hits];
      total = res.total;
    } catch (e) {
      if (seq === searchSeq) error = `Couldn't load more results: ${e}`;
      return;
    } finally {
      if (loadingMoreSeq === seq) loadingMoreSeq = -1;
    }
    const found = followPending();
    await tick();
    if (found >= 0) scrollToTile(found);
    maybeLoadMore(); // still short of the pane's end (or the selection's)?
  }

  /** Loads more if the grid is scrolled near its end (or doesn't overflow yet). */
  function maybeLoadMore() {
    const el = view === "list" ? list : grid;
    if (!el) return;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - LOAD_AHEAD_PX) loadMore();
  }

  async function select(i: number) {
    if (hits.length === 0) return;
    confirmDelete = null;
    selected = Math.max(0, Math.min(hits.length - 1, i));
    await tick();
    scrollToTile(selected); // from the computed layout: the tile may not be rendered yet
    // Arrowing towards the end loads the next page before you hit it.
    if (selected >= hits.length - COLS * 3) loadMore();
  }

  const copy = (i = selected) =>
    act(async () => {
      const hit = hits[i];
      if (hit) await invoke("copy_meme", { path: hit.path }); // hides the pane on success
    });

  /** Instant insert (Shift+Enter / Shift+click): paste into the app you were in. */
  const insert = (i = selected) =>
    act(async () => {
      const hit = hits[i];
      if (hit) await invoke("insert_meme", { path: hit.path }); // hides the pane itself
    });

  /** Show in folder (Ctrl+Shift+F): Explorer/Finder with the meme selected. */
  /** Pins the meme to the top of an empty search (on every device), or unpins it. */
  const togglePin = (i = selected) =>
    act(async () => {
      const hit = hits[i];
      if (!hit) return;
      await invoke("set_pinned", { path: hit.path, pinned: !hit.pinned });
      wantSelected(hit.path); // keep it selected wherever it moves to
      await runSearch(false);
      showToast({ message: `${hit.pinned ? "Unpinned" : "Pinned"} ${stemOf(hit.name)}` }, 2500);
    });

  const reveal = (i = selected) =>
    act(async () => {
      const hit = hits[i];
      if (hit) await invoke("reveal_meme", { path: hit.path });
    });

  /** The most recent delete or rename, undoable with Ctrl+Z (or its toast's button) for a while. */
  let lastUndo: { id: number; run: () => void } | null = null;
  let undoSeq = 0;

  function offerUndo(run: () => void, ms: number) {
    const id = ++undoSeq;
    lastUndo = { id, run };
    setTimeout(() => {
      if (lastUndo?.id === id) lastUndo = null;
    }, ms);
  }
  /** Ctrl+Z works this long after a delete (matches the backend's undo window)... */
  const UNDO_MS = 10_000;
  /** ...while the "Deleted" notice itself gets out of the way sooner. */
  const DELETED_TOAST_MS = 5_000;

  const doDelete = (hit: Hit) =>
    act(async () => {
      confirmDelete = null;
      const token = await invoke<string>("delete_meme", { path: hit.path });
      offerUndo(() => undoDelete(token), UNDO_MS);
      showToast(
        { message: `Deleted ${hit.name}`, danger: true, action: { label: "Undo", run: () => undoDelete(token) } },
        DELETED_TOAST_MS,
      );
      await refresh();
    });

  const undoDelete = (token: string) =>
    act(async () => {
      lastUndo = null;
      toast = null;
      await invoke("undo_delete", { token });
      await refresh();
      showToast({ message: "Restored" }, 2000);
    });

  // ---------- toast ----------

  type Toast = {
    message: string;
    action?: { label: string; run: () => void };
    error?: boolean;
    /** Faded red, for destructive things that happened (e.g. a delete). */
    danger?: boolean;
  };
  let toast = $state<Toast | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  /** A short notice above the action bar ("Added cat.png", "Deleted… Undo"). */
  function showToast(next: Toast, ms = 4000) {
    clearTimeout(toastTimer);
    toast = next;
    toastTimer = setTimeout(() => (toast = null), ms);
  }

  // ---------- adding memes (drop / paste) ----------

  type ImportResult = { added: string[]; duplicates: string[] };
  let dragDepth = $state(0);
  let importing = $state(false);

  const isUrl = (text: string) => /^https?:\/\/\S+$/i.test(text.trim());

  async function addMemes(files: File[], urls: string[]) {
    if (stage !== "ready" || (files.length === 0 && urls.length === 0)) return;
    importing = true;
    const added: string[] = [];
    const duplicates: string[] = [];
    const failures: string[] = [];
    /** The meme to show: the first one added (or found already in the library). */
    let shown: string | null = null;
    const collect = async (r: ImportResult) => {
      added.push(...r.added);
      duplicates.push(...r.duplicates);
      const first = r.added[0] ?? r.duplicates[0];
      if (first && !shown) {
        shown = first;
        await showMeme(first); // right away, while any other files are still being added
      }
    };
    for (const file of files) {
      try {
        const bytes = new Uint8Array(await file.arrayBuffer());
        await collect(await invoke<ImportResult>("import_bytes", bytes, { headers: { "x-filename": encodeURIComponent(file.name) } }));
      } catch (e) {
        failures.push(`${file.name || "Image"}: ${e}`);
      }
    }
    for (const url of urls) {
      try {
        await collect(await invoke<ImportResult>("import_url", { url }));
      } catch (e) {
        failures.push(String(e));
      }
    }
    importing = false;

    // More memes arrived after the first was shown, shifting it: select it again.
    if (shown && added.length + duplicates.length > 1) await showMeme(shown);
    const name = (rel: string) => rel.split("/").pop();
    const parts: string[] = [];
    if (added.length) parts.push(added.length === 1 ? `Added ${name(added[0])}` : `Added ${added.length} memes`);
    if (duplicates.length) parts.push(duplicates.length === 1 ? `${name(duplicates[0])} is already in your library` : `${duplicates.length} were already in your library`);
    if (failures.length) parts.push(failures.length === 1 ? failures[0] : `${failures.length} couldn't be added`);
    if (parts.length) showToast({ message: parts.join(" · "), error: failures.length > 0 && added.length === 0 }, 5000);
  }

  /**
   * Clears the search and selects the meme at `rel` (library-relative) in the current
   * view. An empty search lists recently used memes before the newest, so it may be past
   * the first page: load pages until it turns up.
   */
  async function showMeme(rel: string) {
    query = "";
    confirmDelete = null;
    menu = null;
    wantSelected(rel);
    await runSearch(true);
  }

  function onPaste(e: ClipboardEvent) {
    if (stage !== "ready" || !e.clipboardData) return;
    const files = Array.from(e.clipboardData.files);
    const text = e.clipboardData.getData("text/plain");
    if (files.length) {
      e.preventDefault();
      addMemes(files, []);
    } else if (isUrl(text)) {
      e.preventDefault();
      addMemes([], [text.trim()]);
    }
    // Anything else (plain text) pastes into the search box as usual.
  }

  function dropUrls(dt: DataTransfer) {
    const list = dt.getData("text/uri-list") || dt.getData("text/plain");
    return list
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#") && isUrl(l));
  }

  function onDrop(e: DragEvent) {
    e.preventDefault(); // never let the webview open the dropped file itself
    if (ownDrag() || stage !== "ready" || editing) {
      dragDepth = 0;
      return; // a meme dragged out of the pane and dropped back on it: nothing to do
    }
    dragDepth = 0;
    invoke("set_dragging", { active: false, refocus: true });
    if (!e.dataTransfer) return;
    const files = Array.from(e.dataTransfer.files);
    // Images dragged from a browser come as a URL (sometimes with a file too).
    addMemes(files, files.length ? [] : dropUrls(e.dataTransfer));
  }

  function onInput() {
    pendingSelect = null; // typing a new search: don't jump away from it
    error = null;
    confirmDelete = null;
    menu = null;
    if (stage === "ready") runSearch(true);
  }

  // ---------- actions menu ----------

  const MENU_W = 230;
  /** Gap kept between the menu and the pane's edges. */
  const MENU_MARGIN = 8;
  let menuEl: HTMLDivElement | undefined = $state();

  // Once the menu is on screen, measure it and nudge it back inside the pane: its real
  // height depends on how many actions it has (and fonts), so it's never guessed.
  $effect(() => {
    const m = menu;
    const el = menuEl;
    if (!m || !el) return;
    const { width, height } = el.getBoundingClientRect();
    const top = m.bottom !== undefined ? m.bottom - height : m.y;
    const x = Math.max(MENU_MARGIN, Math.min(m.x, window.innerWidth - width - MENU_MARGIN));
    const y = Math.max(MENU_MARGIN, Math.min(top, window.innerHeight - height - MENU_MARGIN));
    if (x !== m.x || y !== m.y || m.bottom !== undefined) menu = { x, y, index: m.index };
  });

  // Enter / click = the primary action, Shift = the other one (swappable in Settings).
  const pasteFirst = $derived(status?.primary_action !== "copy");
  const primary = (i = selected) => (pasteFirst ? insert(i) : copy(i));
  const secondary = (i = selected) => (pasteFirst ? copy(i) : insert(i));
  const pasteAction = { label: "Paste to App", danger: false, run: (i: number) => insert(i) };
  const copyAction = { label: "Copy to Clipboard", danger: false, run: (i: number) => copy(i) };

  const actions = $derived([
    { ...(pasteFirst ? pasteAction : copyAction), keys: "↵" },
    { ...(pasteFirst ? copyAction : pasteAction), keys: "Shift ↵" },
    {
      label: hits[menu?.index ?? selected]?.pinned ? "Unpin" : "Pin to Top",
      keys: `${MOD} P`,
      danger: false,
      run: (i: number) => togglePin(i),
    },
    { label: "Edit…", keys: `${MOD} E`, danger: false, run: (i: number) => openEditor(i) },
    { label: "Show in Folder", keys: `${MOD} Shift F`, danger: false, run: (i: number) => reveal(i) },
    {
      label: "Show Text",
      keys: `${MOD} I`,
      danger: false,
      run: (i: number) => {
        select(i);
        toggleDetails(true);
      },
    },
    {
      label: "Rename…",
      keys: "F2",
      danger: false,
      run: (i: number) => {
        select(i).then(startRename);
      },
    },
    {
      label: "Edit Text…",
      keys: "",
      danger: false,
      run: (i: number) => {
        select(i).then(startEditText); // waits for the meme's current text before editing
      },
    },
    {
      label: "Delete Meme…",
      keys: `${MOD} ⌫`,
      danger: true,
      run: (i: number) => {
        confirmDelete = hits[i];
      },
    },
  ]);

  function openMenuAt(e: MouseEvent, i: number) {
    e.preventDefault();
    select(i);
    menuSel = 0;
    // At the cursor; moved back inside the pane once its size is known (see the $effect).
    menu = { x: e.clientX, y: e.clientY, index: i };
  }

  /** Ctrl+K: Raycast-style actions panel, anchored above the action bar. */
  function openActions() {
    if (!current) return;
    menuSel = 0;
    // Right-aligned, sitting just above the action bar.
    menu = { x: window.innerWidth - MENU_W - 10, y: 0, bottom: window.innerHeight - 52, index: selected };
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
    if (editing) return; // the editor has its own keys (and Esc never closes it)
    const key = e.key;
    const ctrl = e.ctrlKey || e.metaKey;
    if (isModKey(key) && !e.repeat && stage === "ready") {
      clearTimeout(numbersTimer);
      numbersTimer = setTimeout(() => (showNumbers = true), 250); // only when held, not for Ctrl+K etc.
    } else if (!isModKey(key)) {
      hideNumbers();
    }

    // Typing in an edit field (rename, text): leave the keys to it.
    const target = e.target;
    if (target instanceof HTMLTextAreaElement || (target instanceof HTMLInputElement && target !== input)) return;
    // Ctrl+Z right after a delete brings it back (otherwise it undoes typing as usual).
    if (ctrl && key.toLowerCase() === "z" && !e.shiftKey && lastUndo && stage === "ready") {
      e.preventDefault();
      const undo = lastUndo;
      lastUndo = null;
      undo.run();
      return;
    }
    if (key === "F2" && stage === "ready" && current) {
      e.preventDefault();
      startRename();
      return;
    }

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
      else if (showDetails) toggleDetails(false);
      else if (stage === "ready") invoke("hide"); // the setup window doesn't vanish on Esc
      return;
    }

    // Ctrl+, (Cmd+, on Mac) opens the Settings window, from any screen.
    if (ctrl && key === ",") {
      e.preventDefault();
      invoke("open_settings");
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
    } else if (stage === "ready") {
      if (confirmDelete) {
        if (key === "Enter") { e.preventDefault(); doDelete(confirmDelete); }
        return;
      }
      if (ctrl && /^Digit[1-9]$/.test(e.code)) {
        e.preventDefault();
        const n = Number(e.code.slice(5)) - 1;
        if (hits[n]) {
          hideNumbers();
          select(n);
          if (e.shiftKey) secondary(n);
          else primary(n);
        }
        return;
      }
      if (ctrl && key.toLowerCase() === "l") { e.preventDefault(); toggleView(); return; }
      if (ctrl && key.toLowerCase() === "k") { e.preventDefault(); openActions(); return; }
      if (ctrl && key.toLowerCase() === "i") { e.preventDefault(); toggleDetails(); return; }
      if (ctrl && key.toLowerCase() === "e") { e.preventDefault(); openEditor(); return; }
      if (ctrl && !e.shiftKey && key.toLowerCase() === "p") { e.preventDefault(); togglePin(); return; }
      if (ctrl && e.shiftKey && key.toLowerCase() === "f") { e.preventDefault(); reveal(); return; }
      switch (key) {
        case "ArrowRight": e.preventDefault(); move("right"); break;
        case "ArrowLeft":  e.preventDefault(); move("left"); break;
        case "ArrowDown":  e.preventDefault(); move("down"); break;
        case "ArrowUp":    e.preventDefault(); move("up"); break;
        case "PageDown":   e.preventDefault(); move("down", 3); break;
        case "PageUp":     e.preventDefault(); move("up", 3); break;
        case "Enter":      e.preventDefault(); if (e.shiftKey) secondary(); else primary(); break;
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
    checkDuplicates();
    const unlisten = [
      // A short message from the backend (e.g. "memeity is up to date!" from the tray).
      listen<string>("notice", (e) => showToast({ message: e.payload }, 3000)),
      listen("drag-out-ended", () => {
        draggingOut = false;
        dragOutEndedAt = Date.now();
      }),
      listen("pane-shown", () => {
        hideNumbers();
        draggingOut = false;
        if (editing) return; // reopened mid-edit: leave the editor as it was
        checkDuplicates();
        error = null;
        confirmDelete = null;
        menu = null;
        input?.focus();
        input?.select(); // typing replaces the last query, arrows keep it
        refresh();
      }),
      listen("library-changed", () => refresh()),
      listen("sync-status", () => refresh()),
      listen("status-changed", () => refresh()), // e.g. a new shortcut picked in Settings
      listen("ocr-status", async () => {
        // Progress ticks often; only refresh the status, not the search results.
        try {
          status = await invoke<Status>("get_status");
        } catch (e) {
          error = String(e);
        }
      }),
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
  onkeyup={(e) => {
    if (isModKey(e.key)) hideNumbers();
  }}
  onmousemove={onMouseMove}
  onmouseup={() => (pressed = null)}
  oncontextmenu={onContextMenu}
  onmousedown={(e) => {
    if (menu && !(e.target instanceof Element && e.target.closest(".menu"))) menu = null;
  }}
  onblur={() => {
    menu = null;
    hideNumbers();
  }}
  ondragenter={(e) => {
    // Always claim drags: a drop the page doesn't handle makes the webview open the file
    // as a page (replacing memeity with just the image).
    e.preventDefault();
    if (stage !== "ready" || ownDrag() || editing) return; // our own drag passing over the pane; the editor takes its own drops
    if (dragDepth++ === 0) invoke("set_dragging", { active: true }); // keep the pane open
  }}
  ondragover={(e) => {
    e.preventDefault(); // see ondragenter
    // Our own meme, or not ready for memes: show "can't drop here".
    if (e.dataTransfer && (stage !== "ready" || ownDrag())) e.dataTransfer.dropEffect = "none";
  }}
  ondragleave={() => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) invoke("set_dragging", { active: false });
  }}
  ondrop={onDrop}
/>

{#snippet pinIcon()}
  <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
    <path d="M12 17v5" />
    <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" />
  </svg>
{/snippet}

{#snippet folderIcon()}
  <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
    <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
  </svg>
{/snippet}

<main class:pane={stage === "ready"}>
  <!-- Screen readers hear how many memes match as you type. -->
  {#if stage === "ready"}
    <p class="sr-only" aria-live="polite">{query.trim() ? `${total} ${total === 1 ? "meme" : "memes"}` : ""}</p>
  {/if}
  {#if stage === "ready"}
    <header>
      <input
        bind:this={input}
        bind:value={query}
        oninput={onInput}
        onpaste={onPaste}
        aria-label="Search memes"
        role="combobox"
        aria-expanded="true"
        aria-autocomplete="list"
        aria-controls="results"
        aria-activedescendant={current ? `meme-${selected}` : undefined}
        placeholder={status?.count ? `Search ${status.count} memes by name or text…` : "Search memes…"}
        spellcheck="false"
        autocomplete="off"
      />
      {#if stage === "ready" && status?.drive_folder}
        <button
          class="badge"
          title={`Open ${status.local_folder ?? "the meme folder"}`}
          onmousedown={(e) => e.preventDefault()}
          onclick={() => invoke("open_folder")}
        >
          {@render folderIcon()}
          {status.drive_folder.name}
        </button>
      {/if}
      <button
        class="icon-btn"
        title={`${view === "grid" ? "List" : "Grid"} view (${MOD}${isMac ? "" : "+"}L)`}
        aria-label={view === "grid" ? "Switch to list view" : "Switch to grid view"}
        onmousedown={(e) => e.preventDefault()}
        onclick={toggleView}
      >
        {#if view === "grid"}
          <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            <rect x="3" y="4" width="6" height="6" rx="1" /><path d="M13 6h8M13 9h5" />
            <rect x="3" y="14" width="6" height="6" rx="1" /><path d="M13 16h8M13 19h5" />
          </svg>
        {:else}
          <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            <rect x="3" y="3" width="7" height="9" rx="1" /><rect x="14" y="3" width="7" height="5" rx="1" />
            <rect x="14" y="12" width="7" height="9" rx="1" /><rect x="3" y="16" width="7" height="5" rx="1" />
          </svg>
        {/if}
      </button>
      <button
        class="icon-btn"
        title={`Settings (${MOD}${isMac ? "" : "+"},)`}
        aria-label="Settings"
        onmousedown={(e) => e.preventDefault()}
        onclick={() => invoke("open_settings")}
      >
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
          <circle cx="12" cy="12" r="3" />
        </svg>
      </button>
    </header>
  {/if}

  {#if status?.shortcut_error}
    <div class="banner shortcut-banner" role="alert">
      <span>{status.shortcut_error} Until then, open memeity from its {isMac ? "menu bar" : "tray"} icon.</span>
      <span class="banner-actions">
        <button class="btn sm" onclick={() => invoke("open_settings")}>Choose another…</button>
        <button class="btn ghost sm" onclick={retryShortcut} disabled={busy}>Try again</button>
      </span>
    </div>
  {/if}
  {#if stage === "ready" && duplicateCount > duplicatesDismissedAt}
    <div class="banner shortcut-banner" role="status">
      <span>
        Found {duplicateCount} identical cop{duplicateCount === 1 ? "y" : "ies"} of memes in your library.
      </span>
      <span class="banner-actions">
        <button class="btn sm" onclick={() => invoke("open_settings", { section: "duplicates" })}>Review…</button>
        <button class="btn ghost sm" onclick={dismissDuplicates}>Dismiss</button>
      </span>
    </div>
  {/if}
  {#if error}
    <!-- Full error text (not truncated) and selectable, so it can be copied into a bug report. -->
    <div class="banner error-banner" role="alert">
      <span class="error-text">{error}</span>
      <button
        class="btn ghost sm"
        title="Open a GitHub issue about this (copies diagnostics to paste in)"
        onclick={() => invoke("report_problem", { context: error }).catch((e) => (error = String(e)))}
      >Report…</button>
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
        <img class="logo" src={logo} alt="memeity" width="64" height="64" />
        <h2>Sign in to Google Drive</h2>
        <p>
          memeity keeps your memes in a <b>memeity</b> folder in your Google Drive and syncs it to every device you sign in on.
          It can only see files it creates, never the rest of your Drive.
        </p>
        <p class="callout">
          Your browser will open to sign in.
          Your sign-in stays in {credentialStore}.
        </p>
        <button class="btn primary" onclick={signIn} disabled={busy}>
          {busy ? "Waiting for your browser…" : "Sign in with Google"}
          {#if !busy}<kbd>↵</kbd>{/if}
        </button>
      </div>
    {:else if stage === "folder"}
      <div class="empty">
        <div class="empty-icon">{@render folderIcon()}</div>
        {#if busy || !folderTried}
          <h2>Setting up your memeity folder…</h2>
          <p>Finding (or creating) the <b>memeity</b> folder in your Google Drive.</p>
        {:else}
          <h2>Couldn't reach your memeity folder</h2>
          <p>Check your connection and try again.</p>
          <button class="btn primary" onclick={setupFolder}>Try again</button>
        {/if}
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
            <p>Drop images into your memeity folder on any device and they'll sync everywhere.</p>
            <button class="btn outline" onclick={() => invoke("open_folder")}>Open local folder</button>
          {/if}
        </div>
      {:else if hits.length === 0}
        <div class="empty">
          <h2>No results</h2>
          <p>Nothing matches “{query}”.</p>
        </div>
      {:else if view === "list"}
        <div class="list-view">
          <div
            class="list"
            id="results"
            bind:this={list}
            role="listbox"
            aria-label="Memes"
            onscroll={() => {
              menu = null;
              if (list) listScroll = list.scrollTop;
              maybeLoadMore();
            }}
          >
            <div class="list-rows" style:height="{hits.length * ROW_H + 2 * LIST_PAD}px">
              {#each listRows as i (hits[i].path)}
                {@const hit = hits[i]}
                <button
                  class="row"
                  class:selected={i === selected}
                  class:doomed={confirmDelete?.path === hit.path}
                  role="option"
                  id={`meme-${i}`}
                  aria-selected={i === selected}
                  aria-label={`${stemOf(hit.name)}${hit.pinned ? ", pinned" : ""}`}
                  tabindex="-1"
                  style:top="{LIST_PAD + i * ROW_H}px"
                  title={hit.rel}
                  onmousedown={(e) => pressItem(e, i)}
                  onclick={() => {
                    if (!clickWasDrag()) select(i);
                  }}
                  ondblclick={(e) => (e.shiftKey ? secondary(i) : primary(i))}
                  oncontextmenu={(e) => openMenuAt(e, i)}
                >
                  {#if showNumbers && i < 9}<kbd class="num">{i + 1}</kbd>{/if}
                  <img src={tileSrc(hit, false)} alt="" loading="lazy" decoding="async" draggable="false" />
                  <span class="row-text">
                    <span class="row-name">
                      {#if hit.pinned}<span class="pin-inline" title="Pinned">{@render pinIcon()}</span>{/if}
                      {stemOf(hit.name)}
                    </span>
                    <span class="row-meta">{[fileType(hit), sizeLabel(hit), folderOf(hit)].filter(Boolean).join(" · ")}</span>
                  </span>
                </button>
              {/each}
            </div>
          </div>
          {#if current}
            <div class="list-preview">
              <div class="preview-stage">
                <!-- The cached thumbnail shows at once; the original covers it when loaded. -->
                <img src={tileSrc(current, false)} alt="" draggable="false" />
                {#key current.path}
                  <img src={`${convertFileSrc(current.path, "thumb")}?full=1&v=${current.version}`} alt={current.name} draggable="false" />
                {/key}
              </div>
              <div class="file-row">
                <p class="preview-name" title={current.rel}>{current.name}</p>
                <button class="link-btn" onclick={startRename} title="Rename (F2)">Rename</button>
              </div>
              <p class="facts">
                {[fileType(current) + (current.animated ? " (animated)" : ""), sizeLabel(current)].filter(Boolean).join(" · ")}
              </p>
              <!-- A meme with no text in it gets no text section at all (Edit Text… in the
                   actions menu can still add some). -->
              {#if !(details?.state === "read" && !details.text)}
                <div class="file-row">
                  <p class="label">Text in image</p>
                  {#if details}
                    <button class="link-btn" onclick={startEditText}>{details.text ? "Edit" : "Add text"}</button>
                  {/if}
                </div>
              {/if}
              {#if !details}
                <div class="list-text skeleton-text"></div>
              {:else if details.state === "read" && details.text}
                <div class="list-text">{details.text}</div>
              {:else if details.state === "read"}
                <!-- no text: nothing to show -->
              {:else if details.state === "pending"}
                <p class="note">
                  {status.ocr.state === "unavailable" ? "Text recognition isn't available on this device." : "Not read yet. It's in the queue."}
                </p>
              {:else}
                <p class="note">Couldn't read this image.</p>
              {/if}
            </div>
          {/if}
        </div>
      {:else}
        <div
          class="masonry"
          id="results"
          role="listbox"
          aria-label="Memes"
          bind:this={grid}
          onscroll={() => {
            menu = null;
            if (grid) scrollTop = grid.scrollTop;
            maybeLoadMore();
          }}
        >
          {#each windowed as w, c (c)}
            <div class="col" style:padding-top="{w.padTop}px" style:padding-bottom="{w.padBottom}px">
              {#each w.items as i (hits[i].path)}
                {@const hit = hits[i]}
                {@const active = i === selected || (i === hovered && !reduceMotion)}
                <button
                  class="tile"
                  class:selected={i === selected}
                  class:doomed={confirmDelete?.path === hit.path}
                  data-i={i}
                  role="option"
                  id={`meme-${i}`}
                  aria-selected={i === selected}
                  aria-label={`${stemOf(hit.name)}${hit.pinned ? ", pinned" : ""}`}
                  tabindex="-1"
                  style:aspect-ratio="1 / {ratioOf(hit)}"
                  title={hit.name}
                  onmousedown={(e) => pressItem(e, i)}
                  onclick={(e) => {
                    if (clickWasDrag()) return;
                    // GIF-picker style: one click acts, same as Enter (Shift = Shift+Enter).
                    select(i);
                    if (e.shiftKey) secondary(i);
                    else primary(i);
                  }}
                  oncontextmenu={(e) => openMenuAt(e, i)}
                  onmouseenter={() => (hovered = i)}
                  onmouseleave={() => (hovered = -1)}
                >
                  <img src={tileSrc(hit, active)} alt={hit.name} loading="lazy" decoding="async" draggable="false" />
                  {#if showNumbers && i < 9}<kbd class="num">{i + 1}</kbd>{/if}
                  {#if hit.pinned}<span class="pin-badge" title="Pinned">{@render pinIcon()}</span>{/if}
                </button>
              {/each}
            </div>
          {/each}
        </div>
      {/if}
    {/if}

    {#if stage === "ready" && showDetails && current}
      <div class="details">
        <div class="preview">
          <img src={`${convertFileSrc(current.path, "thumb")}?full=1&v=${current.version}`} alt={current.name} draggable="false" />
        </div>
        <aside class="info">
          <p class="label">File</p>
          {#if renaming}
            <div class="name-edit">
              <input
                bind:this={nameInput}
                bind:value={nameDraft}
                onkeydown={(e) => onEditKey(e, saveRename, false)}
                spellcheck="false"
                aria-label="New name"
              />
              {#if extOf(current.name)}<span class="ext">.{extOf(current.name)}</span>{/if}
            </div>
            <div class="edit-actions">
              <button class="btn ghost sm" onclick={stopEditing}>Cancel</button>
              <button class="btn primary sm" onclick={saveRename} disabled={busy || !nameDraft.trim()}>Rename <kbd>↵</kbd></button>
            </div>
          {:else}
            <div class="file-row">
              <p class="filename" title={current.rel}>{current.rel}</p>
              <button class="link-btn" onclick={startRename} title="Rename (F2)">Rename</button>
            </div>
          {/if}

          <div class="file-row">
            <p class="label">Text in image</p>
            {#if details && !editingText}
              <button class="link-btn" onclick={startEditText}>{details.text ? "Edit" : "Add text"}</button>
            {/if}
          </div>
          {#if editingText}
            <textarea
              class="ocr-text"
              bind:this={textInput}
              bind:value={textDraft}
              onkeydown={(e) => onEditKey(e, saveText, true)}
              spellcheck="true"
              placeholder="Type the text in this meme…"
              aria-label="Text in image"
            ></textarea>
            <div class="edit-actions">
              <button class="btn ghost sm" onclick={stopEditing}>Cancel</button>
              <button class="btn primary sm" onclick={saveText} disabled={busy}>Save <kbd>{MOD}</kbd><kbd>↵</kbd></button>
            </div>
          {:else if !details}
            <div class="ocr-text skeleton-text"></div>
          {:else if details.state === "read" && details.text}
            <div class="ocr-text">{details.text}</div>
          {:else if details.state === "read"}
            <p class="note">No text found in this image.</p>
          {:else if details.state === "pending"}
            <p class="note">
              {status?.ocr.state === "unavailable" ? "Text recognition isn't available on this device." : "Not read yet. It's in the queue."}
            </p>
          {:else}
            <p class="note">Couldn't read this image.</p>
          {/if}

          {#if details?.state === "read" && !editingText}
            <p class="meta">
              {details.edited ? "Edited by you" : "Read"} {formatWhen(details.at)}{details.lang ? ` · ${details.lang}` : ""}
            </p>
          {/if}

          <div class="info-actions">
            <button class="btn outline sm" disabled={!details?.text} onclick={copyText}>
              {copiedText ? "Copied" : "Copy Text"}
            </button>
            <span class="spacer"></span>
            <button class="btn ghost sm" onclick={() => toggleDetails(false)}>Close <kbd>Esc</kbd></button>
          </div>
        </aside>
      </div>
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
        {#if status.ocr.state === "preparing"}
          <span class="divider"></span>
          <span class="status busy" title={status.ocr.message ?? ""}><i class="dot"></i>Getting text search ready…</span>
        {:else if status.ocr.state === "reading"}
          <span class="divider"></span>
          <span class="status busy" title="Reading the text in your memes so you can search it">
            <i class="dot"></i>Reading text · {status.ocr.done}/{status.ocr.total}
          </span>
        {:else if status.ocr.state === "unavailable"}
          <span class="divider"></span>
          <span class="status err" title={status.ocr.message ?? ""}><i class="dot"></i>Text search unavailable</span>
        {/if}
      {:else if status?.account}
        <span class="status muted">{status.account}</span>
      {:else}
        <span class="status muted">memeity</span>
      {/if}
      <span class="spacer"></span>

      {#if stage === "ready" && current}
        <button class="action primary-action" onclick={() => primary()}>
          {#if pasteFirst}
            <svg class="icon action-icon" viewBox="0 0 24 24" aria-hidden="true">
              <path d="M15 2H9a1 1 0 0 0-1 1v2c0 .6.4 1 1 1h6c.6 0 1-.4 1-1V3c0-.6-.4-1-1-1Z" />
              <path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2M16 4h2a2 2 0 0 1 2 2v2M11 14h10" />
              <path d="m17 10 4 4-4 4" />
            </svg>
            Paste to App <kbd>↵</kbd>
          {:else}
            <svg class="icon action-icon" viewBox="0 0 24 24" aria-hidden="true">
              <rect x="8" y="2" width="8" height="4" rx="1" />
              <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
            </svg>
            Copy to Clipboard <kbd>↵</kbd>
          {/if}
        </button>
        <button class="action" onclick={() => secondary()}>
          {pasteFirst ? "Copy" : "Paste"} <kbd>Shift</kbd><kbd>↵</kbd>
        </button>
        <span class="divider"></span>
        <button class="action" onclick={openActions}>Actions <kbd>{MOD}</kbd><kbd>K</kbd></button>
      {/if}
    {/if}
  </footer>

  {#if dragDepth > 0}
    <div class="dropzone" aria-hidden="true">
      <div>
        <strong>Drop to add to memeity</strong>
        <span>Images, GIFs, or links to them</span>
      </div>
    </div>
  {/if}

  {#if editing && Editor}
    <Editor hit={editing} {pasteFirst} onclose={closeEditor} />
  {/if}

  {#if importing || toast}
    <div
      class="toast"
      class:err={toast?.error}
      class:danger={toast?.danger}
      class:has-action={!importing && !!toast?.action}
      role="status"
    >
      <span>{importing ? "Adding…" : toast?.message}</span>
      {#if !importing && toast?.action}
        <button
          onclick={() => {
            toast?.action?.run();
            toast = null;
          }}
        >{toast.action.label}</button>
      {/if}
    </div>
  {/if}
</main>

{#if menu}
  <div
    class="menu"
    role="menu"
    bind:this={menuEl}
    style:left="{menu.x}px"
    style:top="{menu.y}px"
    style:width="{MENU_W}px"
    style:visibility={menu.bottom !== undefined ? "hidden" : null}
  >
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
    position: relative;
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
    height: 26px;
    padding: 0 8px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    cursor: pointer;
    transition: background 100ms ease, color 100ms ease;
  }

  .badge:hover {
    background: var(--accent);
    color: var(--fg);
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    flex-shrink: 0;
    padding: 0;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    cursor: pointer;
    transition: background 100ms ease, color 100ms ease;
  }

  .icon-btn:hover {
    background: var(--accent);
    color: var(--fg);
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

  .shortcut-banner {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .banner-actions {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
    margin-left: auto;
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
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
  }

  /* ---------- details view (Ctrl+I) ---------- */

  .details {
    position: absolute;
    inset: 0;
    z-index: 5;
    display: grid;
    grid-template-columns: 1fr 280px;
    background: var(--bg);
  }

  .preview {
    display: grid;
    place-items: center;
    min-width: 0;
    min-height: 0;
    padding: 20px;
    background: var(--bar);
  }

  .preview img {
    max-width: 100%;
    max-height: 100%;
    border-radius: 8px;
    object-fit: contain;
  }

  .info {
    display: flex;
    flex-direction: column;
    min-height: 0;
    padding: 18px 18px 14px;
    border-left: 1px solid var(--border);
  }

  .label {
    margin: 0 0 4px;
    font-size: 11px;
    font-weight: 500;
    color: var(--muted-fg);
  }

  .filename {
    margin: 0 0 16px;
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .ocr-text {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    font: 12.5px/1.55 "Geist Mono Variable", ui-monospace, monospace;
    white-space: pre-wrap;
    word-break: break-word;
    user-select: text;
    -webkit-user-select: text;
    cursor: text;
  }

  .skeleton-text {
    animation: pulse 1.4s ease-in-out infinite;
  }

  .note {
    flex: 1;
    margin: 0;
    font-size: 13px;
    color: var(--muted-fg);
  }

  .meta {
    margin: 8px 0 0;
    font-size: 11px;
    color: var(--muted-fg);
  }

  .file-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    min-width: 0;
  }

  .file-row .filename {
    min-width: 0;
  }

  .link-btn {
    flex-shrink: 0;
    padding: 0;
    border: none;
    background: none;
    color: var(--muted-fg);
    font-size: 11px;
    font-weight: 500;
    text-decoration: underline;
    text-underline-offset: 2px;
    cursor: pointer;
  }

  .link-btn:hover {
    color: var(--fg);
  }

  .name-edit {
    display: flex;
    align-items: center;
    height: 32px;
    margin-bottom: 8px;
    padding: 0 8px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: var(--muted);
  }

  .name-edit input {
    flex: 1;
    min-width: 0;
    font-size: 13px;
  }

  .name-edit .ext {
    color: var(--muted-fg);
    font-size: 13px;
  }

  textarea.ocr-text {
    width: 100%;
    resize: none;
    color: var(--fg);
    border-color: var(--ring);
    outline: none;
  }

  .edit-actions {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
    margin: 8px 0 14px;
  }

  .info-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 12px;
  }

  /* ---------- masonry ---------- */

  /* Ctrl/Cmd held: which number key acts on which result. */
  .pin-badge {
    position: absolute;
    top: 6px;
    right: 6px;
    z-index: 1;
    display: grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.6);
    color: #fff;
    box-shadow: 0 1px 4px rgb(0 0 0 / 0.3);
  }

  .pin-badge .icon {
    width: 13px;
    height: 13px;
  }

  .pin-inline {
    display: inline-flex;
    vertical-align: -2px;
    margin-right: 4px;
    color: var(--muted-fg);
  }

  .pin-inline .icon {
    width: 14px;
    height: 14px;
  }

  .num {
    position: absolute;
    top: 6px;
    left: 6px;
    z-index: 1;
    min-width: 20px;
    padding: 1px 6px;
    border-radius: 6px;
    background: var(--fg);
    color: var(--bg);
    font-size: 11px;
    font-weight: 600;
    text-align: center;
    box-shadow: 0 1px 4px rgb(0 0 0 / 0.3);
  }

  .row .num {
    position: static;
    flex-shrink: 0;
    box-shadow: none;
  }

  /* ---------- list view ---------- */

  .list-view {
    flex: 1;
    display: flex;
    min-width: 0;
    min-height: 0;
  }

  .list {
    position: relative;
    width: 42%;
    flex-shrink: 0;
    overflow-y: auto;
    border-right: 1px solid var(--border);
  }

  .list-rows {
    position: relative;
  }

  .row {
    position: absolute;
    left: 8px;
    right: 8px;
    height: 56px; /* ROW_H minus the 4px between rows */
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 0 8px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: var(--fg);
    text-align: left;
    cursor: pointer;
  }

  .row:hover {
    background: color-mix(in srgb, var(--muted) 60%, transparent);
  }

  .row.selected {
    background: var(--muted);
    box-shadow: inset 0 0 0 1px var(--ring);
  }

  @media (forced-colors: active) {
    .row.selected {
      outline: 2px solid Highlight;
      outline-offset: -2px;
    }

    .tile.selected {
      outline-color: Highlight;
    }
  }

  .row.doomed {
    box-shadow: inset 0 0 0 1px var(--destructive);
  }

  .row img {
    width: 42px;
    height: 42px;
    flex-shrink: 0;
    border-radius: 6px;
    object-fit: cover;
    background: var(--muted);
  }

  .row-text {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }

  .row-name,
  .row-meta {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }

  .row-name {
    font-size: 15.5px;
    font-weight: 500;
    letter-spacing: -0.01em;
  }

  .row-meta {
    font-size: 12.5px;
    color: var(--muted-fg);
  }

  .list-preview {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 16px 20px;
    /* Doesn't scroll: the preview takes whatever height the details leave, so the image
       always has a definite box to fit into. */
    overflow: hidden;
  }

  .preview-stage {
    position: relative;
    flex: 1 1 0;
    /* Never less than half the column, however long the details below get. */
    min-height: 50%;
    border-radius: 10px;
    background: var(--muted);
    overflow: hidden;
  }

  /* Pinned to the box and scaled to fit whole (letterboxed), whatever its proportions. */
  .preview-stage img {
    position: absolute;
    inset: 8px;
    width: calc(100% - 16px);
    height: calc(100% - 16px);
    object-fit: contain;
  }

  .preview-name {
    margin: 0;
    overflow: hidden;
    font-size: 14px;
    font-weight: 500;
    white-space: nowrap;
    text-overflow: ellipsis;
  }

  .list-preview > :not(.preview-stage) {
    flex-shrink: 0;
  }

  /* When space runs short, the text box gives way (it scrolls), never the preview. */
  .list-preview > .list-text {
    flex-shrink: 1;
    min-height: 2.6em;
  }

  .facts {
    margin: -6px 0 0; /* reads as a subtitle of the name above */
    font-size: 12px;
    color: var(--muted-fg);
  }

  .list-text {
    max-height: 84px;
    flex-shrink: 0;
    overflow-y: auto;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    font: 12px/1.5 "Geist Mono Variable", ui-monospace, monospace;
    white-space: pre-wrap;
    word-break: break-word;
    user-select: text;
    -webkit-user-select: text;
  }

  .list-preview .note {
    flex: none; /* the preview image takes the spare height, not the note */
  }

  .list-preview .label {
    margin: 0;
  }

  .list-text.skeleton-text {
    height: 32px;
  }

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

  .empty .logo {
    display: block;
    margin: 0 auto 16px;
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

  .empty b {
    color: var(--fg);
    font-weight: 500;
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

  .action:hover {
    background: var(--accent);
    color: var(--fg);
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

  /* ---------- drop overlay & toast ---------- */

  main {
    position: relative;
  }

  .dropzone {
    position: absolute;
    inset: 8px;
    z-index: 20;
    display: grid;
    place-items: center;
    border: 2px dashed var(--ring);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg) 85%, transparent);
    backdrop-filter: blur(2px);
    pointer-events: none;
    text-align: center;
  }

  .dropzone strong {
    display: block;
    font-size: 16px;
    font-weight: 600;
  }

  .dropzone span {
    color: var(--muted-fg);
    font-size: 13px;
  }

  .toast {
    position: absolute;
    /* Centred with auto margins (not a translate), so text never lands on half pixels. */
    left: 20px;
    right: 20px;
    bottom: 56px;
    z-index: 15;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 12px;
    width: fit-content;
    max-width: calc(100% - 40px);
    height: 38px;
    margin: 0 auto;
    padding: 0 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--popover);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.35);
    font-size: 13px;
  }

  /* With a button (Undo), tighten the right side so the button sits evenly in the corner. */
  .toast.has-action {
    padding-right: 6px;
  }

  .toast span {
    line-height: 1.3; /* room for descenders (g, y, p), which overflow:hidden would clip */
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .toast.err span {
    color: var(--destructive);
  }

  .toast.danger {
    border-color: color-mix(in srgb, var(--destructive) 35%, var(--border));
    background: color-mix(in srgb, var(--destructive) 14%, var(--popover));
  }

  .toast.danger span {
    color: color-mix(in srgb, var(--destructive) 80%, var(--fg));
  }

  .toast button {
    flex-shrink: 0;
    height: 26px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg);
    color: var(--fg);
    font-size: 12px;
    font-weight: 500;
    cursor: pointer;
  }

  .toast button:hover {
    background: var(--accent);
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
