<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount, tick } from "svelte";
  import logo from "./assets/logo.png";
  import {
    accentOf,
    accentsOf,
    applyTheme,
    DEFAULT_CHOICE,
    swatchesOf,
    THEMES,
    themeById,
    variantOf,
    type Theme,
    type ThemeChoice,
  } from "./themes";

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
    /** A newer published version, if one is waiting. */
    update: string | null;
    primary_action: "paste" | "copy";
    pane_scale: number;
    scroll_reset_secs: number;
    theme: ThemeChoice | null;
    version: string;
  };

  const sections = [
    { id: "general", label: "General" },
    { id: "appearance", label: "Appearance" },
    { id: "tags", label: "Tags" },
    { id: "account", label: "Account & Sync" },
    { id: "text", label: "Text Search" },
    { id: "duplicates", label: "Duplicates" },
    { id: "about", label: "About" },
  ] as const;
  type Section = (typeof sections)[number]["id"];

  const isMac = navigator.userAgent.includes("Mac");

  // ---------- search ----------

  /** A setting you can search for. `find` is the start of its heading or button text
   * (or `sel` a selector), to scroll to and highlight; neither shows the section. */
  type Entry = { section: Section; title: string; words: string; find?: string; sel?: string };
  const ENTRIES: Entry[] = [
    { section: "general", title: "Shortcut to open memeity", words: "hotkey keyboard key binding pane global", find: "Open memeity" },
    { section: "general", title: "Enter key", words: "return copy paste insert send", find: "Enter key" },
    { section: "general", title: "Pane size", words: "window width height bigger smaller large", find: "Pane size" },
    { section: "general", title: "Scroll back to top", words: "reset position remember", find: "Scroll back to top" },
    { section: "general", title: "Launch at login", words: "startup start boot autostart open automatically", find: "Launch at login" },
    {
      section: "appearance",
      title: "Theme",
      words:
        "color colour scheme dark light mode brightness flavor flavour style catppuccin latte frappe macchiato mocha dracula everforest gruvbox kanagawa nord one onedark rose pine rosepine solarized tokyo night",
      sel: ".theme-grid",
    },
    { section: "appearance", title: "Accent color", words: "colour highlight tint", find: "Accent" },
    { section: "tags", title: "Create a tag", words: "new add label", sel: "form.row" },
    { section: "tags", title: "Rename, recolor or delete tags", words: "edit color colour remove labels" },
    { section: "account", title: "Google account", words: "sign in out login logout email drive" },
    { section: "account", title: "Sync", words: "google drive upload download status cloud", find: "Sync" },
    { section: "account", title: "Memes folder", words: "location path directory drive local", find: "Memes folder" },
    { section: "text", title: "Text in images", words: "ocr read recognition language words", find: "Text in images" },
    { section: "text", title: "Re-read all memes", words: "ocr reread rescan reindex refresh", find: "Re-read all" },
    { section: "duplicates", title: "Duplicates", words: "dupes identical copies similar clean up delete" },
    { section: "about", title: "Check for updates", words: "update upgrade version new release", find: "Check for Updates" },
    { section: "about", title: "Report a problem", words: "bug issue feedback help broken github", find: "Report a Problem" },
    { section: "about", title: "Copy diagnostics", words: "logs debug bug", find: "Copy diagnostics" },
    { section: "about", title: "Website", words: "homepage link", find: "Website" },
    { section: "about", title: "Privacy policy", words: "data telemetry", find: "Privacy policy" },
    { section: "about", title: "Source code", words: "github open source repository", find: "Source code" },
  ];

  let search = $state("");
  let searchInput = $state<HTMLInputElement | undefined>();
  /** The result Enter opens (moved with the arrow keys). */
  let active = $state(0);

  /** Every word typed starts a word of the entry (its title, keywords or section). */
  const results = $derived.by(() => {
    const typed = search.toLowerCase().split(/[\s&,/-]+/).filter(Boolean);
    if (!typed.length) return [];
    const label = (e: Entry) => sections.find((s) => s.id === e.section)?.label ?? "";
    return ENTRIES.map((e) => {
      const title = e.title.toLowerCase().split(/[\s&,/-]+/);
      const all = [...title, ...`${e.words} ${label(e)}`.toLowerCase().split(/[\s&,/-]+/)];
      if (!typed.every((t) => all.some((w) => w.startsWith(t)))) return null;
      // Title matches first.
      return { e, rank: typed.filter((t) => title.some((w) => w.startsWith(t))).length };
    })
      .filter((r) => r !== null)
      .sort((a, b) => b.rank - a.rank)
      .map((r) => r.e);
  });

  $effect(() => {
    void results;
    active = 0;
  });

  /** Shows an entry's section, then scrolls to its setting and highlights it. */
  async function openEntry(e: Entry) {
    section = e.section;
    await tick();
    const content = document.querySelector(".content");
    let el: Element | null = null;
    if (e.sel) el = content?.querySelector(e.sel) ?? null;
    else if (e.find) {
      const hit = [...(content?.querySelectorAll("h3, button") ?? [])].find((n) => n.textContent?.trim().startsWith(e.find!));
      el = hit?.tagName === "H3" ? (hit.closest(".row, .card") ?? hit) : (hit ?? null);
    }
    if (!el) {
      content?.scrollTo({ top: 0 });
      return;
    }
    el.scrollIntoView({ block: "center", behavior: "smooth" });
    el.classList.remove("flash");
    void (el as HTMLElement).offsetWidth; // restart the animation
    el.classList.add("flash");
    setTimeout(() => el.classList.remove("flash"), 1600);
  }

  function onSearchKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (search) {
        e.preventDefault();
        search = "";
      } else searchInput?.blur();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (!results.length) return;
      e.preventDefault();
      active = (active + (e.key === "ArrowDown" ? 1 : results.length - 1)) % results.length;
    } else if (e.key === "Enter" && results[active]) {
      e.preventDefault();
      openEntry(results[active]);
    }
  }

  let status = $state<Status | null>(null);
  // Opened at a section (e.g. from the pane's duplicates prompt): `index.html#duplicates`.
  const sectionFrom = (id: string): Section | null =>
    sections.some((s) => s.id === id) ? (id as Section) : null;
  let section = $state<Section>(sectionFrom(location.hash.slice(1)) ?? "general");
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

  let diagnosticsCopied = $state(false);
  const copyDiagnostics = () =>
    act(async () => {
      await invoke("copy_diagnostics");
      diagnosticsCopied = true;
      setTimeout(() => (diagnosticsCopied = false), 2000);
    });

  const reportProblem = () => act(() => invoke("report_problem"));

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

  /** Windows: memeity is capturing the next combination system-wide (see winhook.rs). */
  let nativeCapture = false;

  function stopRecording() {
    recording = false;
    preview = [];
    if (nativeCapture) invoke("stop_shortcut_capture");
    nativeCapture = false;
  }

  /** The shortcut to save, or a message if it isn't usable. */
  function saveShortcut(accelerator: string) {
    recording = false;
    nativeCapture = false;
    act(async () => {
      status = await invoke<Status>("set_shortcut", { accelerator });
    });
  }

  async function startRecording() {
    error = null;
    preview = [];
    recording = true;
    // Windows: also catch combinations Windows would act on itself (Alt+Space, Win+...).
    nativeCapture = await invoke<boolean>("start_shortcut_capture").catch(() => false);
    // Keys are read from the whole window, so the recorder button doesn't need focus.
    // Keeping it would let the Space of a shortcut like Ctrl+Space "click" it again on
    // key-up and restart recording, as if the new shortcut never took.
    if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
  }

  function onKeydown(e: KeyboardEvent) {
    // Cmd+F / Ctrl+F searches settings.
    if (!recording && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
      e.preventDefault();
      searchInput?.focus();
      searchInput?.select();
      return;
    }
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
      stopRecording();
      return;
    }
    const mods = modifierLabels(e);
    if (MODIFIER_CODES.some((m) => e.code.startsWith(m))) {
      preview = mods; // still holding modifiers
      return;
    }
    // Capturing system-wide: the finished combination arrives as `shortcut-captured`.
    if (nativeCapture) return;
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
    saveShortcut(accelerator);
  }

  /** A combination captured system-wide (Windows), or null if Esc cancelled it. */
  function onCaptured(accelerator: string | null) {
    if (!recording) return;
    nativeCapture = false;
    if (!accelerator) {
      stopRecording();
      return;
    }
    const parts = accelerator.split("+");
    const key = parts[parts.length - 1];
    const has = (m: string) => parts.includes(m);
    preview = [
      ...modifierLabels({ ctrlKey: has("control"), altKey: has("alt"), shiftKey: has("shift"), metaKey: has("super") } as KeyboardEvent),
      keyLabel(key),
    ];
    if (!has("control") && !has("alt") && !has("super")) {
      error = `Include ${isMac ? "Ctrl, Option or Cmd" : "Ctrl, Alt or Win"} so the shortcut doesn't clash with typing.`;
      recording = false;
      return;
    }
    saveShortcut(accelerator);
  }

  // ---------- actions ----------

  const setPrimaryAction = (action: "paste" | "copy") =>
    act(async () => {
      status = await invoke<Status>("set_primary_action", { action });
    });

  const PANE_SCALES = [75, 90, 100, 125, 150];
  const setPaneScale = (percent: number) =>
    act(async () => {
      status = await invoke<Status>("set_pane_scale", { percent });
    });

  const SCROLL_RESETS = [
    { secs: 0, label: "0s" },
    { secs: 30, label: "30s" },
    { secs: 60, label: "1m" },
    { secs: 90, label: "90s" },
    { secs: 180, label: "3m" },
  ];
  const setScrollReset = (secs: number) =>
    act(async () => {
      status = await invoke<Status>("set_scroll_reset", { secs });
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

  // ---------- updates ----------

  let checking = $state(false);
  let upToDate = $state(false);

  async function checkUpdate() {
    checking = true;
    upToDate = false;
    error = null;
    try {
      const version = await invoke<string | null>("check_for_update");
      upToDate = version === null;
      await refresh();
    } catch (e) {
      error = `Couldn't check for updates: ${e}`;
    } finally {
      checking = false;
    }
  }

  // Downloads, installs and restarts memeity (the window closes on success).
  const installUpdate = () => act(() => invoke("install_update"));

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
  type Duplicates = { groups: DupGroup[]; pending: number; exact_extras: number };

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

  // Removing every identical copy at once asks for a second click to confirm.
  let confirmRemoveAll = $state(false);
  let removedAll = $state<number | null>(null);
  const removeAllExact = () =>
    act(async () => {
      if (!confirmRemoveAll) {
        confirmRemoveAll = true;
        return;
      }
      confirmRemoveAll = false;
      removedAll = await invoke<number>("remove_exact_duplicates");
      await loadDupes();
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

  // ---------- appearance ----------

  $effect(() => {
    if (status) applyTheme(status.theme);
  });

  const choice = $derived(status?.theme ?? DEFAULT_CHOICE);
  const theme = $derived(themeById(choice.theme));
  const variant = $derived(variantOf(theme, choice.variant));
  const accent = $derived(accentOf(theme, choice.accent));

  const saveTheme = (next: ThemeChoice) =>
    act(async () => {
      status = await invoke<Status>("set_theme", next);
    });

  /** Switching themes keeps a light/dark choice the new theme also has. */
  function pickTheme(t: Theme) {
    if (t.id === theme.id) return;
    const keep = t.variants.some((v) => v.id === variant.id) ? variant.id : t.defaultVariant;
    saveTheme({ theme: t.id, variant: keep, accent: null });
  }

  const pickVariant = (id: string) => saveTheme({ theme: theme.id, variant: id, accent: choice.accent });
  const pickAccent = (name: string) => saveTheme({ theme: theme.id, variant: variant.id, accent: name });

  /** "rosewater" -> "Rosewater"; "" (no accent) -> "Mono". */
  const label = (name: string) => (name ? name[0].toUpperCase() + name.slice(1) : "Mono");

  // ---------- tags ----------

  type Tag = { id: string; name: string; color: string; count: number };
  /** Same order as tags.rs COLORS. */
  const TAG_COLORS = ["blue", "green", "yellow", "orange", "red", "pink", "purple", "teal", "gray"];

  let tags = $state<Tag[] | null>(null);
  let newTag = $state("");
  /** The tag being renamed, its draft name, and the one whose colours are showing. */
  let renamingTag = $state<string | null>(null);
  let tagDraft = $state("");
  let coloring = $state<string | null>(null);
  /** Deleting asks for a second click. */
  let confirmDeleteTag = $state<string | null>(null);

  async function loadTags() {
    try {
      tags = await invoke<Tag[]>("list_tags");
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    if (section === "tags" && !tags) loadTags();
  });

  const createTag = () =>
    act(async () => {
      if (!newTag.trim()) return;
      await invoke("create_tag", { name: newTag });
      newTag = "";
      await loadTags();
    });

  function startRenameTag(t: Tag) {
    renamingTag = t.id;
    tagDraft = t.name;
    coloring = null;
    confirmDeleteTag = null;
  }

  const saveTagName = (t: Tag) =>
    act(async () => {
      if (tagDraft.trim() && tagDraft.trim() !== t.name) {
        tags = await invoke<Tag[]>("update_tag", { id: t.id, name: tagDraft });
      }
      renamingTag = null;
    });

  const setTagColor = (t: Tag, color: string) =>
    act(async () => {
      tags = await invoke<Tag[]>("update_tag", { id: t.id, color });
      coloring = null;
    });

  const deleteTag = (t: Tag) =>
    act(async () => {
      if (confirmDeleteTag !== t.id) {
        confirmDeleteTag = t.id;
        renamingTag = null;
        coloring = null;
        return;
      }
      confirmDeleteTag = null;
      tags = await invoke<Tag[]>("delete_tag", { id: t.id });
    });

  /** Focuses (and selects) an input as soon as it appears. */
  function autofocus(el: HTMLInputElement) {
    el.focus();
    el.select();
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
    const unlisten = ["sync-status", "ocr-status", "library-changed", "update-status"].map((e) =>
      listen(e, () => refresh()),
    );
    unlisten.push(listen<string | null>("shortcut-captured", (e) => onCaptured(e.payload)));
    // Tags changed in the pane or on another device; counts change with the library.
    unlisten.push(listen("tags-changed", () => tags && loadTags()));
    unlisten.push(listen("library-changed", () => tags && loadTags()));
    unlisten.push(
      listen<string>("show-section", (e) => {
        const next = sectionFrom(e.payload);
        if (next) section = next;
      }),
    );
    const timer = setInterval(() => (now = Date.now()), 5000);
    return () => {
      if (recording) stopRecording(); // never leave system-wide capture on
      clearInterval(timer);
      unlisten.forEach((p) => p.then((f) => f()));
    };
  });
</script>

<svelte:window
  onkeydown={onKeydown}
  onblur={() => {
    // Clicked away: stop recording, so a system-wide capture never eats keys meant for another app.
    if (recording) stopRecording();
  }}
/>

<div class="settings">
  <nav class="side" aria-label="Settings sections">
    <div class="brand"><img src={logo} alt="" width="22" height="22" />Settings</div>
    <div class="side-search">
      <svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg>
      <input
        bind:this={searchInput}
        bind:value={search}
        onkeydown={onSearchKey}
        type="search"
        placeholder="Search"
        aria-label="Search settings"
        aria-controls="setting-results"
        spellcheck="false"
        autocomplete="off"
      />
      {#if !search}<kbd>{isMac ? "⌘F" : "Ctrl+F"}</kbd>{/if}
    </div>
    {#if search.trim()}
      <div id="setting-results" class="results" role="listbox" aria-label="Matching settings">
        {#each results as r, i (r.section + r.title)}
          <button
            class="result"
            class:active={i === active}
            role="option"
            aria-selected={i === active}
            onmouseenter={() => (active = i)}
            onclick={() => openEntry(r)}
          >
            <span>{r.title}</span>
            <span class="where">{sections.find((s) => s.id === r.section)?.label}</span>
          </button>
        {:else}
          <p class="no-results">No matching settings</p>
        {/each}
      </div>
    {:else}
      {#each sections as s (s.id)}
        <button
          class:active={section === s.id}
          aria-current={section === s.id ? "page" : undefined}
          onclick={() => (section = s.id)}
        >{s.label}</button>
      {/each}
    {/if}
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
          <button
            class="recorder"
            class:recording
            onclick={startRecording}
            onkeyup={(e) => {
              // Belt and braces for the above: Space/Enter never re-trigger it from the keyboard.
              if (e.key === " " || e.key === "Enter") e.preventDefault();
            }}
            disabled={busy}
          >
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
            <h3>Pane size</h3>
            <p>How big the search pane is, compared to the default.</p>
          </div>
          <div class="segmented" role="radiogroup" aria-label="Pane size">
            {#each PANE_SCALES as p (p)}
              <button
                role="radio"
                aria-checked={status.pane_scale === p}
                class:on={status.pane_scale === p}
                onclick={() => setPaneScale(p)}
                disabled={busy}
              >{p}%</button>
            {/each}
          </div>
        </div>
      </div>

      <div class="card">
        <div class="row">
          <div>
            <h3>Scroll back to top</h3>
            <p>If the pane stays closed this long, it reopens at the top of your memes.</p>
          </div>
          <div class="segmented" role="radiogroup" aria-label="Scroll back to top">
            {#each SCROLL_RESETS as r (r.secs)}
              <button
                role="radio"
                aria-checked={status.scroll_reset_secs === r.secs}
                class:on={status.scroll_reset_secs === r.secs}
                onclick={() => setScrollReset(r.secs)}
                disabled={busy}
              >{r.label}</button>
            {/each}
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
    {:else if section === "appearance"}
      <h1>Appearance</h1>

      <div class="theme-grid" role="radiogroup" aria-label="Theme">
        {#each THEMES as t (t.id)}
          {@const v = t.id === theme.id ? variant : variantOf(t, t.defaultVariant)}
          {@const sw = swatchesOf(t, v, t.id === theme.id ? choice.accent : null)}
          <button
            class="theme-card"
            class:on={t.id === theme.id}
            role="radio"
            aria-checked={t.id === theme.id}
            onclick={() => pickTheme(t)}
            disabled={busy}
          >
            <span class="theme-preview" style:background={sw[0]} style:color={sw[1]}>
              <span class="preview-text">Aa</span>
              <span class="preview-accent" style:background={sw[2]}></span>
              <span class="preview-dots">
                {#each sw.slice(3) as c, n (n)}<i style:background={c}></i>{/each}
              </span>
            </span>
            <span class="theme-name">{t.name}</span>
          </button>
        {/each}
      </div>

      {#if theme.variants.length > 1}
        <div class="card">
          <div class="row">
            <div>
              <h3>{theme.variantLabel ?? "Variant"}</h3>
              <p>{theme.id === "default" ? "System follows your computer's light or dark mode." : `${theme.name} comes in ${theme.variants.length} styles.`}</p>
            </div>
            <div class="segmented" role="radiogroup" aria-label={theme.variantLabel ?? "Variant"}>
              {#each theme.variants as v (v.id)}
                <button
                  role="radio"
                  aria-checked={variant.id === v.id}
                  class:on={variant.id === v.id}
                  onclick={() => pickVariant(v.id)}
                  disabled={busy}
                >{v.name}</button>
              {/each}
            </div>
          </div>
        </div>
      {/if}

      <div class="card">
        <h3>Accent</h3>
        <p class="muted small">Colours buttons, the selected meme and focus rings.</p>
        <div class="accents" role="radiogroup" aria-label="Accent">
          {#each accentsOf(theme) as name (name)}
            <button
              class="accent-swatch"
              class:on={(accent ?? "") === name}
              role="radio"
              aria-checked={(accent ?? "") === name}
              aria-label={label(name)}
              title={label(name)}
              onclick={() => pickAccent(name)}
              disabled={busy}
            >
              <i style:background={name ? variant.palette.colors[name] : variant.palette.fg}></i>
            </button>
          {/each}
        </div>
        <p class="muted small accent-name">{label(accent ?? "")}</p>
      </div>
    {:else if section === "tags"}
      <h1>Tags</h1>

      <div class="card">
        <form class="row" onsubmit={(e) => { e.preventDefault(); createTag(); }}>
          <input
            class="text-input"
            bind:value={newTag}
            placeholder="New tag name"
            aria-label="New tag name"
            spellcheck="false"
            autocomplete="off"
            maxlength="64"
          />
          <button class="btn primary sm" type="submit" disabled={busy || !newTag.trim()}>Create tag</button>
        </form>
        <p class="note">
          In the search pane, {isMac ? "⌘" : "Ctrl"}-click memes to select several (Shift-click selects a range), then press
          <kbd>{isMac ? "⌘" : "Ctrl"}</kbd><kbd>T</kbd> to tag them. Search with <span class="mono-inline">#tag</span>.
        </p>
      </div>

      {#if !tags}
        <p class="muted">Loading…</p>
      {:else if tags.length === 0}
        <div class="card">
          <h3>No tags yet</h3>
          <p class="muted small">Tags you make here or in the search pane show up on every device.</p>
        </div>
      {:else}
        <div class="card tag-list">
          {#each tags as t (t.id)}
            <div class="tag-row">
              <button
                class="swatch"
                data-color={t.color}
                class:open={coloring === t.id}
                title="Change colour"
                aria-label={`Colour of ${t.name}: ${t.color}`}
                onclick={() => {
                  coloring = coloring === t.id ? null : t.id;
                  renamingTag = null;
                  confirmDeleteTag = null;
                }}
              ><span class="tag-dot" data-color={t.color}></span></button>
              {#if renamingTag === t.id}
                <form class="tag-rename" onsubmit={(e) => { e.preventDefault(); saveTagName(t); }}>
                  <input
                    class="text-input"
                    bind:value={tagDraft}
                    use:autofocus
                    onkeydown={(e) => e.key === "Escape" && (renamingTag = null)}
                    aria-label={`Rename ${t.name}`}
                    spellcheck="false"
                    autocomplete="off"
                    maxlength="64"
                  />
                  <button class="btn ghost sm" type="button" onclick={() => (renamingTag = null)}>Cancel</button>
                  <button class="btn primary sm" type="submit" disabled={busy || !tagDraft.trim()}>Save</button>
                </form>
              {:else}
                <button class="tag-name" title="Rename" onclick={() => startRenameTag(t)}>
                  <span class="tag-chip" data-color={t.color}>{t.name}</span>
                </button>
                <span class="muted small count">{t.count} meme{t.count === 1 ? "" : "s"}</span>
                <span class="buttons">
                  <button class="btn ghost sm" onclick={() => startRenameTag(t)}>Rename</button>
                  <button class="btn ghost sm danger" onclick={() => deleteTag(t)} disabled={busy}>
                    {confirmDeleteTag === t.id ? "Delete? Click again" : "Delete"}
                  </button>
                </span>
              {/if}
            </div>
            {#if coloring === t.id}
              <div class="swatches" role="radiogroup" aria-label={`Colour of ${t.name}`}>
                {#each TAG_COLORS as c (c)}
                  <button
                    class="swatch"
                    class:on={t.color === c}
                    role="radio"
                    aria-checked={t.color === c}
                    aria-label={c}
                    title={c}
                    onclick={() => setTagColor(t, c)}
                    disabled={busy}
                  ><span class="tag-dot" data-color={c}></span></button>
                {/each}
              </div>
            {/if}
          {/each}
        </div>
        <p class="note top">Deleting a tag takes it off every meme, on every device. The memes stay.</p>
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

      {#if removedAll !== null}
        <div class="undo-bar" role="status">
          <span>
            Removed {removedAll} identical cop{removedAll === 1 ? "y" : "ies"}. They're in Google Drive's trash for 30 days
            if you need one back.
          </span>
          <button class="btn outline sm" onclick={() => (removedAll = null)}>OK</button>
        </div>
      {/if}

      {#if dupes && dupes.exact_extras > 0}
        <div class="card remove-all">
          <div>
            <h3>{dupes.exact_extras} identical cop{dupes.exact_extras === 1 ? "y" : "ies"}</h3>
            <p class="muted small">
              Byte-for-byte the same as another meme. Removing them keeps the best copy of each (highest resolution, then
              oldest).
            </p>
          </div>
          <button class="btn sm danger" onclick={removeAllExact} disabled={busy}>
            {confirmRemoveAll ? `Remove ${dupes.exact_extras}? Click again` : "Remove copies"}
          </button>
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
        <div class="about-actions">
          {#if status.update}
            <button class="btn primary sm wide" onclick={installUpdate} disabled={busy}>
              {busy ? "Updating…" : `Update to ${status.update}`}
            </button>
          {:else}
            <button class="btn outline sm wide" onclick={checkUpdate} disabled={busy || checking}>
              {checking ? "Checking…" : "Check for Updates"}
            </button>
          {/if}
          {#if upToDate}<span class="muted small wide">memeity is up to date.</span>{/if}
          <button class="btn outline sm" onclick={() => openLink("website")}>Website</button>
          <button class="btn outline sm" onclick={() => openLink("privacy")}>Privacy policy</button>
          <button class="btn outline sm" onclick={() => openLink("source")}>Source code</button>
          <button class="btn outline sm wide" onclick={reportProblem} disabled={busy}>
            <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
              <path d="m8 2 1.88 1.88" /><path d="M14.12 3.88 16 2" />
              <path d="M9 7.13v-1a3.003 3.003 0 1 1 6 0v1" />
              <path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6" />
              <path d="M12 20v-9" /><path d="M6.53 9C4.6 8.8 3 7.1 3 5" /><path d="M6 13H2" />
              <path d="M3 21c0-2.1 1.7-3.9 3.8-4" /><path d="M20.97 5c0 2.1-1.6 3.8-3.5 4" />
              <path d="M22 13h-4" /><path d="M17.2 17c2.1.1 3.8 1.9 3.8 4" />
            </svg>
            Report a Problem…
          </button>
        </div>
        <div class="diagnostics">
          <button class="btn ghost sm" onclick={copyDiagnostics} disabled={busy}>
            {diagnosticsCopied ? "Copied" : "Copy diagnostics"}
          </button>
          <p class="muted small">
            Report a Problem opens a new GitHub issue (public) and copies these diagnostics for you to paste in:
            version, sync status and recent log lines. Check them first: they include file names from your meme
            folder, but not your Google account.
          </p>
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

  .side-search {
    position: relative;
    display: flex;
    align-items: center;
    margin: 0 0 10px;
  }

  .side-search .icon {
    position: absolute;
    left: 8px;
    width: 13px;
    height: 13px;
    color: var(--muted-fg);
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    pointer-events: none;
  }

  .side-search input {
    width: 100%;
    height: 28px;
    padding: 0 8px 0 27px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg);
    color: var(--fg);
    font: inherit;
    font-size: 12.5px;
    outline: none;
  }

  .side-search input::-webkit-search-cancel-button {
    display: none;
  }

  .side-search input:focus {
    border-color: var(--border-strong);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--ring) 15%, transparent);
  }

  .side-search kbd {
    position: absolute;
    right: 6px;
    color: var(--muted-fg);
    font: 10.5px "Geist Mono Variable", ui-monospace, monospace;
    pointer-events: none;
  }

  .results {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-height: 0;
    overflow-y: auto;
  }

  .side .result {
    display: flex;
    flex-direction: column;
    gap: 1px;
    color: var(--fg);
  }

  .result .where {
    color: var(--muted-fg);
    font-size: 11px;
  }

  .side .result.active {
    background: var(--accent);
  }

  .no-results {
    margin: 4px 10px;
    color: var(--muted-fg);
    font-size: 12px;
  }

  /* A setting found by search: briefly outlined. */
  :global(.flash) {
    animation: flash 1.6s ease-out;
  }

  @keyframes flash {
    0%,
    40% {
      box-shadow: 0 0 0 2px var(--ring);
      border-radius: 10px;
    }
    100% {
      box-shadow: 0 0 0 2px transparent;
      border-radius: 10px;
    }
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

  .remove-all {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 12px;
  }

  .remove-all h3 {
    margin: 0 0 2px;
  }

  .remove-all .btn {
    flex-shrink: 0;
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

  /* The button, with "memeity is up to date" (when shown) centred underneath it. */
  /* One block of equal-width buttons: the three links share the width of the widest,
     and the single buttons span the whole row, so every edge lines up. */
  .about-actions {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
    width: max-content;
    margin: 0 auto;
  }

  .about-actions .btn {
    justify-content: center;
  }

  .about-actions .icon {
    width: 14px;
    height: 14px;
    flex-shrink: 0;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .about-actions .wide {
    grid-column: 1 / -1;
  }

  .about .about-actions .muted {
    margin: -2px 0 2px;
  }

  .about {
    padding-top: 8px; /* on top of .content's own padding */
    text-align: center;
  }

  .about h1 {
    margin: 12px 0 2px;
  }

  .about .muted {
    margin: 0 0 18px;
    font-size: 13px;
  }

  .diagnostics {
    margin-top: 28px;
  }

  .about .diagnostics .muted {
    max-width: 380px;
    margin: 6px auto 0;
    font-size: 11px;
  }

  /* ---------- tags ---------- */

  .text-input {
    flex: 1;
    min-width: 0;
    height: 30px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg);
    color: var(--fg);
    font: inherit;
    font-size: 13px;
    outline: none;
  }

  .text-input:focus {
    border-color: var(--border-strong);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--ring) 15%, transparent);
  }

  .mono-inline {
    font: 12px "Geist Mono Variable", ui-monospace, monospace;
  }

  .note kbd {
    vertical-align: 1px;
  }

  .tag-list {
    padding: 6px 8px;
  }

  .tag-row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 40px;
  }

  .tag-row + .tag-row,
  .swatches + .tag-row {
    border-top: 1px solid var(--border);
  }

  .swatch {
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    flex-shrink: 0;
    padding: 0;
    border: 1px solid transparent;
    border-radius: 6px;
    background: none;
    cursor: pointer;
  }

  .swatch .tag-dot {
    width: 12px;
    height: 12px;
  }

  .swatch:hover:not(:disabled),
  .swatch.open {
    background: var(--accent);
  }

  .swatch.on {
    border-color: var(--ring);
  }

  .swatches {
    display: flex;
    gap: 4px;
    padding: 0 0 8px 30px;
  }

  .tag-name {
    min-width: 0;
    padding: 0;
    border: none;
    background: none;
    cursor: pointer;
  }

  .tag-name .tag-chip {
    height: 22px;
    font-size: 12.5px;
  }

  .count {
    flex: 1;
    white-space: nowrap;
  }

  .tag-rename {
    display: flex;
    flex: 1;
    gap: 6px;
  }

  /* ---------- appearance ---------- */

  .theme-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(118px, 1fr));
    gap: 10px;
    margin-bottom: 14px;
  }

  .theme-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 6px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: none;
    text-align: left;
    cursor: pointer;
  }

  .theme-card:hover:not(:disabled) {
    border-color: var(--border-strong);
  }

  .theme-card.on {
    border-color: var(--ring);
    box-shadow: 0 0 0 1px var(--ring);
  }

  .theme-preview {
    position: relative;
    display: block;
    height: 58px;
    border-radius: 6px;
    box-shadow: inset 0 0 0 1px rgb(127 127 127 / 0.25);
    overflow: hidden;
  }

  .preview-text {
    position: absolute;
    top: 8px;
    left: 10px;
    font-size: 17px;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .preview-accent {
    position: absolute;
    top: 12px;
    right: 10px;
    width: 26px;
    height: 12px;
    border-radius: 999px;
  }

  .preview-dots {
    position: absolute;
    bottom: 9px;
    left: 10px;
    display: flex;
    gap: 4px;
  }

  .preview-dots i {
    width: 9px;
    height: 9px;
    border-radius: 50%;
  }

  .theme-name {
    padding: 0 2px 2px;
    font-size: 12.5px;
    font-weight: 500;
  }

  .accents {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 10px;
  }

  .accent-swatch {
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    padding: 0;
    border: 1px solid transparent;
    border-radius: 50%;
    background: none;
    cursor: pointer;
  }

  .accent-swatch i {
    width: 20px;
    height: 20px;
    border-radius: 50%;
    box-shadow: inset 0 0 0 1px rgb(127 127 127 / 0.3);
  }

  .accent-swatch:hover:not(:disabled) {
    border-color: var(--border-strong);
  }

  .accent-swatch.on {
    border: 2px solid var(--ring);
  }

  .accent-name {
    margin: 6px 0 0;
  }
</style>
