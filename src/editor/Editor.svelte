<script lang="ts">
  // The quick editor, inside the pane (which grows while it's open). Loaded on demand,
  // so the search pane never pays for it. Saves the result as a new meme; the original
  // is left alone.
  import { invoke } from "@tauri-apps/api/core";
  import { onDestroy, onMount } from "svelte";
  import { Engine, FONTS, isShapeTool, type ShapeStyle, type Tool } from "./engine";

  type Hit = { path: string; name: string };
  type ImportResult = { added: string[]; duplicates: string[] };

  let {
    hit,
    pasteFirst,
    onclose,
  }: {
    hit: Hit;
    /** Same as the pane: Ctrl+Enter pastes (true) or copies; Ctrl+Shift+Enter does the other. */
    pasteFirst: boolean;
    onclose: (saved?: { rel: string; duplicate: boolean }) => void;
  } = $props();

  const isMac = navigator.userAgent.includes("Mac");
  const MOD = isMac ? "⌘" : "Ctrl+";
  const SWATCHES = ["#ffffff", "#000000", "#ff2d2d", "#ffd400", "#22c55e", "#3b82f6", "#a855f7", "#ff8ad8"];

  let host: HTMLDivElement | undefined = $state();
  let canvasEl: HTMLCanvasElement | undefined = $state();
  let stickerInput: HTMLInputElement | undefined = $state();
  let engine = $state.raw<Engine | null>(null);
  /** Bumped whenever the engine's state changes, so the UI re-reads it. */
  let rev = $state(0);
  let loading = $state(true);
  let saving = $state(false);
  let error = $state<string | null>(null);
  /** The "leave the editor?" prompt: Enter leaves, Esc stays. */
  let confirmDiscard = $state(false);
  /** Whether leaving now would lose edits (read when the prompt opens). */
  let hasEdits = $state(false);
  const urls: string[] = [];

  const stemOf = (name: string) => (name.lastIndexOf(".") > 0 ? name.slice(0, name.lastIndexOf(".")) : name);
  const ext = $derived(hit.name.slice(hit.name.lastIndexOf(".") + 1).toLowerCase());
  /** Photos stay JPEG (a PNG of a photo is huge); everything else is PNG. */
  const asJpeg = $derived(ext === "jpg" || ext === "jpeg");
  const animated = $derived(ext === "gif" || ext === "webp");

  // What the panel shows, re-read from the engine on every change.
  const ui = $derived.by(() => {
    rev;
    const e = engine;
    if (!e) return null;
    const kind = e.selectedKind();
    return {
      tool: e.tool,
      count: e.selected().length,
      kind,
      text: { ...e.textStyle },
      textSize: e.textSize(),
      shape: { ...e.shapeStyle },
      arrow: { ...e.arrowStyle },
      brushColor: e.brushColor,
      brushSize: e.brushSize,
      eraserSize: e.eraserSize,
      canUndo: e.canUndo(),
      canRedo: e.canRedo(),
    };
  });

  function changed() {
    // A new selection: the panel shows (and edits) its style.
    engine?.readSelectedStyle();
    rev++;
  }

  onMount(() => {
    let observer: ResizeObserver | undefined;
    (async () => {
      try {
        const bytes = await invoke<ArrayBuffer>("read_meme", { path: hit.path });
        const url = URL.createObjectURL(new Blob([bytes]));
        urls.push(url);
        const img = new Image();
        img.src = url;
        await img.decode();
        if (!canvasEl || !host) return;
        engine = new Engine(canvasEl, img, { onChange: changed });
        const fit = () => host && engine?.fit(host.clientWidth - 40, host.clientHeight - 40);
        fit();
        observer = new ResizeObserver(fit);
        observer.observe(host);
        loading = false;
        rev++;
      } catch (e) {
        loading = false;
        error = `Couldn't open this meme: ${e}`;
      }
    })();
    return () => observer?.disconnect();
  });

  onDestroy(() => {
    engine?.dispose();
    urls.forEach((u) => URL.revokeObjectURL(u));
  });

  // ---------- actions ----------

  function setTool(tool: Tool) {
    engine?.setTool(tool);
  }

  function addSticker(file: File) {
    if (!engine || !file.type.startsWith("image/")) return;
    const url = URL.createObjectURL(file);
    urls.push(url); // kept until the editor closes: undo may bring the sticker back
    engine.addSticker(url).catch((e) => (error = `Couldn't add that image: ${e}`));
  }

  function pickedStickers(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    Array.from(input.files ?? []).forEach(addSticker);
    input.value = "";
  }

  function askToLeave() {
    hasEdits = engine?.dirty() ?? false;
    confirmDiscard = true;
  }

  function cancel() {
    if (engine?.dirty()) askToLeave();
    else onclose();
  }

  async function save() {
    if (!engine || saving) return;
    saving = true;
    error = null;
    try {
      const bytes = await engine.export(asJpeg ? "image/jpeg" : "image/png");
      const name = `${stemOf(hit.name)} (edited).${asJpeg ? "jpg" : "png"}`;
      const result = await invoke<ImportResult>("import_bytes", bytes, {
        headers: { "x-filename": encodeURIComponent(name) },
      });
      const rel = result.added[0] ?? result.duplicates[0];
      if (!rel) throw new Error("Nothing was saved.");
      onclose({ rel, duplicate: !result.added.length });
    } catch (e) {
      error = `Couldn't save: ${e}`;
      saving = false;
    }
  }

  /** Copies the result, or pastes it into the app you were in, without saving it. */
  async function share(action: "copy" | "paste") {
    if (!engine || saving) return;
    saving = true;
    error = null;
    try {
      const bytes = await engine.export(asJpeg ? "image/jpeg" : "image/png");
      const name = `${stemOf(hit.name)} (edited).${asJpeg ? "jpg" : "png"}`;
      await invoke("share_edited", bytes, {
        headers: { "x-action": action, "x-filename": encodeURIComponent(name) },
      });
      onclose();
    } catch (e) {
      error = `Couldn't ${action}: ${e}`;
      saving = false;
    }
  }

  // ---------- keyboard, paste, drop ----------

  function onKeydown(e: KeyboardEvent) {
    const key = e.key;
    const ctrl = e.ctrlKey || e.metaKey;
    if (ctrl && key.toLowerCase() === "s") {
      e.preventDefault();
      save();
      return;
    }
    if (confirmDiscard) {
      if (key === "Escape" && !e.repeat) { e.preventDefault(); confirmDiscard = false; }
      if (key === "Enter") { e.preventDefault(); onclose(); }
      return;
    }
    // Typing (a text object, or a field in the panel): the keys are theirs.
    const t = e.target;
    if (t instanceof HTMLTextAreaElement || (t instanceof HTMLInputElement && t.type !== "range" && t.type !== "color")) return;
    if (!engine) {
      if (key === "Escape") { e.preventDefault(); onclose(); }
      return;
    }
    const k = key.toLowerCase();

    if (key === "Escape") {
      e.preventDefault();
      // Stop typing / deselect / back to the select tool; with nothing left to undo
      // that way, ask whether to leave the editor.
      if (!engine.escape() && !e.repeat) askToLeave(); // (holding Esc doesn't open and close it)
      return;
    }
    if (ctrl) {
      const handled = () => e.preventDefault();
      if (k === "z" && !e.shiftKey) { handled(); engine.undo(); }
      else if ((k === "z" && e.shiftKey) || k === "y") { handled(); engine.redo(); }
      else if (k === "d") { handled(); engine.duplicateSelected(); }
      else if (k === "a") { handled(); engine.selectAll(); }
      else if (key === "]") { handled(); engine.arrange(e.shiftKey ? "front" : "forward"); }
      else if (key === "[") { handled(); engine.arrange(e.shiftKey ? "back" : "backward"); }
      else if (key === "Enter") { handled(); share(pasteFirst !== e.shiftKey ? "paste" : "copy"); }
      return;
    }
    if (e.altKey) return;
    const step = e.shiftKey ? 10 : 1;
    switch (key) {
      case "Delete":
      case "Backspace": e.preventDefault(); engine.deleteSelected(); return;
      case "ArrowLeft": e.preventDefault(); engine.nudge(-step, 0); return;
      case "ArrowRight": e.preventDefault(); engine.nudge(step, 0); return;
      case "ArrowUp": e.preventDefault(); engine.nudge(0, -step); return;
      case "ArrowDown": e.preventDefault(); engine.nudge(0, step); return;
      case "[":
      case "]": {
        e.preventDefault();
        const grow = key === "]" ? 1.25 : 0.8;
        if (engine.tool === "eraser") setEraserSize(engine.eraserSize * grow);
        else setBrushSize(engine.brushSize * grow);
        return;
      }
    }
    const tools: Record<string, () => void> = {
      v: () => setTool("select"),
      b: () => setTool("brush"),
      e: () => setTool("eraser"),
      t: () => engine?.addText(),
      r: () => setTool("rect"),
      o: () => setTool("ellipse"),
      a: () => setTool("arrow"),
      i: () => stickerInput?.click(),
    };
    if (tools[k] && !e.repeat) {
      e.preventDefault();
      tools[k]();
    }
  }

  function onPaste(e: ClipboardEvent) {
    const t = e.target;
    if (t instanceof HTMLTextAreaElement || t instanceof HTMLInputElement) return; // pasting text into a text object
    const files = Array.from(e.clipboardData?.files ?? []).filter((f) => f.type.startsWith("image/"));
    if (files.length) {
      e.preventDefault();
      files.forEach(addSticker);
    }
  }

  function onDrop(e: DragEvent) {
    e.preventDefault();
    Array.from(e.dataTransfer?.files ?? []).forEach(addSticker);
  }

  // ---------- panel ----------

  const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

  function setBrushSize(v: number) {
    if (!engine) return;
    engine.brushSize = clamp(Math.round(v), 1, 120);
    engine.updateCursor();
    rev++;
  }

  function setEraserSize(v: number) {
    if (!engine) return;
    engine.eraserSize = clamp(Math.round(v), 2, 120);
    engine.updateCursor();
    rev++;
  }

  function setBrushColor(c: string) {
    if (!engine) return;
    engine.brushColor = c;
    if (engine.tool !== "brush") engine.setTool("brush");
    rev++;
  }

  /** Changes the selected shapes' style (or, while drawing shapes, the next one's). */
  function setShape(kind: string | undefined, patch: Partial<ShapeStyle>) {
    engine?.setShapeStyle(kind === "arrow" ? "arrow" : "shape", patch);
    rev++;
  }

  const shapeTools = [
    { tool: "rect", label: "Rectangle", key: "R" },
    { tool: "ellipse", label: "Circle", key: "O" },
    { tool: "arrow", label: "Arrow", key: "A" },
  ] as const;

  const tools = [
    { tool: "select", label: "Select & move", key: "V" },
    { tool: "brush", label: "Draw", key: "B" },
    { tool: "eraser", label: "Erase drawing", key: "E" },
  ] as const;
</script>

<svelte:window onkeydown={onKeydown} onpaste={onPaste} />

{#snippet icon(name: string)}
  <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
    {#if name === "select"}<path d="M4 4l7.07 17 2.51-7.39L21 11.07z" />
    {:else if name === "brush"}<path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z" />
    {:else if name === "eraser"}<path d="m7 21-4.3-4.3c-1-1-1-2.5 0-3.4l9.6-9.6c1-1 2.5-1 3.4 0l5.6 5.6c1 1 1 2.5 0 3.4L13 21" /><path d="M22 21H7M5 11l9 9" />
    {:else if name === "text"}<path d="M4 7V4h16v3M9 20h6M12 4v16" />
    {:else if name === "rect"}<rect x="3" y="5" width="18" height="14" rx="2" />
    {:else if name === "ellipse"}<circle cx="12" cy="12" r="9" />
    {:else if name === "arrow"}<path d="M5 12h14M12 5l7 7-7 7" />
    {:else if name === "sticker"}<rect x="3" y="3" width="18" height="18" rx="2" /><circle cx="9" cy="9" r="2" /><path d="m21 15-3.09-3.09a2 2 0 0 0-2.82 0L6 21" />
    {:else if name === "undo"}<path d="M3 7v6h6" /><path d="M21 17a9 9 0 0 0-9-9 9 9 0 0 0-6 2.3L3 13" />
    {:else if name === "redo"}<path d="M21 7v6h-6" /><path d="M3 17a9 9 0 0 1 9-9 9 9 0 0 1 6 2.3l3 2.7" />
    {:else if name === "back"}<path d="m15 18-6-6 6-6" />
    {/if}
  </svg>
{/snippet}

<!-- Fill, outline and outline width for a shape (selected, or the next one drawn). -->
{#snippet shapeFields(kind: string, s: ShapeStyle)}
  <div class="field">
    <span class="label">Fill</span>
    {@render colors(s.fill, (c) => setShape(kind, { fill: c }), true)}
  </div>
  <div class="field">
    <span class="label">Outline</span>
    {@render colors(s.outline, (c) => setShape(kind, { outline: c }), true)}
  </div>
  {#if s.outline}
    <div class="field">
      <span class="label">Outline width <span class="val">{s.outlineWidth}</span></span>
      <input type="range" aria-label="Outline width" min="1" max="40" value={s.outlineWidth} oninput={(e) => setShape(kind, { outlineWidth: +e.currentTarget.value })} />
    </div>
  {/if}
{/snippet}

<!-- A colour row: swatches, a custom colour, and (optionally) "none". -->
{#snippet colors(value: string | null, pick: (c: string | null) => void, allowNone: boolean)}
  <div class="swatches">
    {#if allowNone}
      <button class="swatch none" class:on={value === null} aria-pressed={value === null} title="None" aria-label="None" onclick={() => pick(null)}></button>
    {/if}
    {#each SWATCHES as c (c)}
      <button
        class="swatch"
        class:on={value?.toLowerCase() === c}
        aria-pressed={value?.toLowerCase() === c}
        style:background={c}
        title={c}
        aria-label={c}
        onclick={() => pick(c)}
      ></button>
    {/each}
    <label class="swatch custom" title="Custom colour" class:on={value !== null && !SWATCHES.includes(value.toLowerCase())}>
      <input type="color" aria-label="Custom colour" value={value ?? "#ffffff"} oninput={(e) => pick(e.currentTarget.value)} />
    </label>
  </div>
{/snippet}

<div class="editor">
  <!-- Drag the empty parts of the bar to move the pane. -->
  <header data-tauri-drag-region>
    <button class="btn ghost" onclick={cancel} title="Cancel (your original stays as it is)">
      {@render icon("back")} Cancel
    </button>
    <span class="title" title={hit.name} data-tauri-drag-region>Editing <strong>{hit.name}</strong></span>
    <span class="spacer" data-tauri-drag-region></span>
    <button class="icon-btn" title={`Undo (${MOD}Z)`} aria-label="Undo" disabled={!ui?.canUndo} onclick={() => engine?.undo()}>
      {@render icon("undo")}
    </button>
    <button class="icon-btn" title={`Redo (${MOD}${isMac ? "⇧Z" : "Y"})`} aria-label="Redo" disabled={!ui?.canRedo} onclick={() => engine?.redo()}>
      {@render icon("redo")}
    </button>
    <button
      class="btn outline-top"
      onclick={() => share(pasteFirst ? "paste" : "copy")}
      disabled={!ui || saving}
      title={`${pasteFirst ? "Paste into the app you were in" : "Copy"} without saving (${isMac ? "⌘" : "Ctrl+"}Enter; add Shift to ${pasteFirst ? "copy" : "paste"} instead)`}
    >
      {pasteFirst ? "Paste to App" : "Copy"} <kbd>{isMac ? "⌘" : "Ctrl"}</kbd><kbd>↵</kbd>
    </button>
    <button class="btn primary" onclick={save} disabled={!ui || saving}>
      {saving ? "Saving…" : "Save as New Meme"} <kbd>{isMac ? "⌘" : "Ctrl"}</kbd><kbd>S</kbd>
    </button>
  </header>

  <div class="body">
    <nav class="tools" aria-label="Tools">
      {#each tools as t (t.tool)}
        <button
          class="tool"
          class:on={ui?.tool === t.tool}
          aria-pressed={ui?.tool === t.tool}
          title={`${t.label} (${t.key})`}
          aria-label={t.label}
          onclick={() => setTool(t.tool)}
        >{@render icon(t.tool)}</button>
      {/each}
      <span class="sep"></span>
      <button class="tool" title="Add text (T)" aria-label="Add text" onclick={() => engine?.addText()}>{@render icon("text")}</button>
      {#each shapeTools as t (t.tool)}
        <button
          class="tool"
          class:on={ui?.tool === t.tool}
          aria-pressed={ui?.tool === t.tool}
          title={`${t.label}: drag to draw, Shift for ${t.tool === "arrow" ? "45° angles" : t.tool === "rect" ? "a square" : "a circle"} (${t.key})`}
          aria-label={`Draw ${t.label.toLowerCase()}`}
          onclick={() => setTool(t.tool)}
        >{@render icon(t.tool)}</button>
      {/each}
      <button class="tool" title="Add image sticker (I), or paste / drop one" aria-label="Add image sticker" onclick={() => stickerInput?.click()}>{@render icon("sticker")}</button>
      <input bind:this={stickerInput} type="file" accept="image/*" multiple hidden onchange={pickedStickers} />
    </nav>

    <div
      class="stage"
      bind:this={host}
      role="region"
      aria-label="Meme canvas. Add things with the tools on the left; select one to change it in the panel on the right."
      ondragover={(e) => e.preventDefault()}
      ondrop={onDrop}
    >
      <div class="frame" class:hidden={loading || (!!error && !engine)}>
        <canvas bind:this={canvasEl}></canvas>
      </div>
      {#if loading}<div class="note">Opening…</div>{/if}
    </div>

    <aside class="panel">
      {#if ui}
        {#if ui.tool === "brush"}
          <h3>Draw</h3>
          <div class="field">
            <span class="label">Colour</span>
            {@render colors(ui.brushColor, (c) => c && setBrushColor(c), false)}
          </div>
          <div class="field">
            <span class="label">Size <span class="val">{ui.brushSize}</span></span>
            <input type="range" aria-label="Brush size" min="1" max="120" value={ui.brushSize} oninput={(e) => setBrushSize(+e.currentTarget.value)} />
          </div>
          <p class="hint">Drawing stays under text, shapes and stickers. <kbd>[</kbd> <kbd>]</kbd> change the size.</p>
        {:else if ui.tool === "eraser"}
          <h3>Erase drawing</h3>
          <div class="field">
            <span class="label">Size <span class="val">{ui.eraserSize}</span></span>
            <input type="range" aria-label="Eraser size" min="2" max="120" value={ui.eraserSize} oninput={(e) => setEraserSize(+e.currentTarget.value)} />
          </div>
          <p class="hint">Only erases drawing. Text, shapes and stickers aren't affected; delete those with <kbd>Del</kbd>.</p>
          <button class="btn outline wide" onclick={() => engine?.clearDrawing()}>Clear all drawing</button>
        {:else if isShapeTool(ui.tool)}
          <h3>{ui.tool === "rect" ? "Rectangle" : ui.tool === "ellipse" ? "Circle" : "Arrow"}</h3>
          {@render shapeFields(ui.tool, ui.tool === "arrow" ? ui.arrow : ui.shape)}
          <p class="hint">
            Drag on the meme to draw one. Hold <kbd>Shift</kbd> for {ui.tool === "arrow" ? "45° angles" : ui.tool === "rect" ? "a square" : "a circle"},
            or just click for a standard size.
          </p>
        {:else if ui.count === 0}
          <h3>Edit</h3>
          <p class="hint">Add text, shapes or stickers from the left, or draw on the meme. Click something to change it; drag its handles to resize or rotate (<kbd>Shift</kbd> keeps proportions).</p>
          <dl class="keys">
            <dt>Edit text</dt><dd>Double-click</dd>
            <dt>Duplicate</dt><dd><kbd>{MOD}D</kbd></dd>
            <dt>Delete</dt><dd><kbd>Del</kbd></dd>
            <dt>Undo / Redo</dt><dd><kbd>{MOD}Z</kbd> <kbd>{MOD}{isMac ? "⇧Z" : "Y"}</kbd></dd>
            <dt>Sticker</dt><dd>Paste or drop an image</dd>
          </dl>
          {#if animated}<p class="hint">Animated memes are saved as a still image.</p>{/if}
        {:else}
          {#if ui.kind === "text"}
            <h3>Text</h3>
            <p class="hint">Drag a corner to stretch it; hold <kbd>Shift</kbd> to keep its proportions.</p>
            <div class="field">
              <span class="label">Font</span>
              <div class="seg">
                {#each FONTS as f (f)}
                  <button class:on={ui.text.font === f} aria-pressed={ui.text.font === f} style:font-family={f} style:font-weight={f === "Barlow Condensed" ? 800 : null} onclick={() => engine?.setTextStyle({ font: f })}>{f === "Barlow Condensed" ? "Barlow" : f}</button>
                {/each}
              </div>
            </div>
            {#if ui.textSize !== null}
              <div class="field">
                <span class="label">Size <span class="val">{ui.textSize}</span></span>
                <input type="range" aria-label="Text size" min="8" max="240" value={ui.textSize} oninput={(e) => engine?.setTextSize(+e.currentTarget.value)} />
              </div>
            {/if}
            <div class="field">
              <span class="label">Colour</span>
              {@render colors(ui.text.fill, (c) => c && engine?.setTextStyle({ fill: c }), false)}
            </div>
            <div class="field">
              <span class="label">Outline</span>
              {@render colors(ui.text.outlineWidth > 0 ? ui.text.outline : null, (c) => engine?.setTextStyle(c ? { outline: c, outlineWidth: ui.text.outlineWidth || 5 } : { outlineWidth: 0 }), true)}
            </div>
            {#if ui.text.outlineWidth > 0}
              <div class="field">
                <span class="label">Outline width <span class="val">{ui.text.outlineWidth}</span></span>
                <input type="range" aria-label="Outline width" min="1" max="10" value={ui.text.outlineWidth} oninput={(e) => engine?.setTextStyle({ outlineWidth: +e.currentTarget.value })} />
              </div>
            {/if}
            <div class="field row">
              <div class="seg">
                <button class:on={ui.text.bold} aria-pressed={ui.text.bold} title="Bold" onclick={() => engine?.setTextStyle({ bold: !ui.text.bold })}><strong>B</strong></button>
                <button class:on={ui.text.italic} aria-pressed={ui.text.italic} title="Italic" onclick={() => engine?.setTextStyle({ italic: !ui.text.italic })}><em>I</em></button>
              </div>
              <div class="seg">
                {#each ["left", "center", "right"] as const as a (a)}
                  <button class:on={ui.text.align === a} aria-pressed={ui.text.align === a} title={`Align ${a}`} aria-label={`Align ${a}`} onclick={() => engine?.setTextStyle({ align: a })}>
                    <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
                      {#if a === "left"}<path d="M3 6h18M3 12h12M3 18h15" />
                      {:else if a === "center"}<path d="M3 6h18M6 12h12M5 18h14" />
                      {:else}<path d="M3 6h18M9 12h12M6 18h15" />{/if}
                    </svg>
                  </button>
                {/each}
              </div>
            </div>
          {:else if ui.kind === "rect" || ui.kind === "ellipse" || ui.kind === "arrow"}
            <h3>{ui.kind === "rect" ? "Rectangle" : ui.kind === "ellipse" ? "Circle" : "Arrow"}{ui.count > 1 ? `s (${ui.count})` : ""}</h3>
            {@render shapeFields(ui.kind, ui.kind === "arrow" ? ui.arrow : ui.shape)}
            <p class="hint">Drag a corner to stretch it; hold <kbd>Shift</kbd> to keep its proportions.</p>
          {:else}
            <h3>{ui.kind === "sticker" ? (ui.count > 1 ? `${ui.count} stickers` : "Sticker") : `${ui.count} selected`}</h3>
          {/if}

          <div class="field">
            <span class="label">Arrange</span>
            <div class="seg">
              <button title={`Bring to front (${MOD}Shift+])`} onclick={() => engine?.arrange("front")}>Front</button>
              <button title={`Bring forward (${MOD}])`} onclick={() => engine?.arrange("forward")}>Up</button>
              <button title={`Send backward (${MOD}[)`} onclick={() => engine?.arrange("backward")}>Down</button>
              <button title={`Send to back (${MOD}Shift+[)`} onclick={() => engine?.arrange("back")}>Back</button>
            </div>
          </div>
          <div class="field row">
            <button class="btn outline" onclick={() => engine?.duplicateSelected()}>Duplicate</button>
            <button class="btn outline danger" onclick={() => engine?.deleteSelected()}>Delete</button>
          </div>
        {/if}
      {/if}
    </aside>
  </div>

  {#if error}
    <div class="toast err" role="alert">
      <span>{error}</span>
      <button onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  {#if confirmDiscard}
    <div class="scrim" role="presentation" onclick={() => (confirmDiscard = false)}>
      <div class="dialog" role="alertdialog" aria-label={hasEdits ? "Discard your edits?" : "Leave the editor?"} tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
        <h3>{hasEdits ? "Discard your edits?" : "Leave the editor?"}</h3>
        <p>{hasEdits ? "Your original meme stays as it is." : "You haven't changed anything yet."}</p>
        <div class="actions">
          <button class="btn outline" onclick={() => (confirmDiscard = false)}>Keep Editing <kbd>Esc</kbd></button>
          <button class={hasEdits ? "btn destructive" : "btn primary"} onclick={() => onclose()}>{hasEdits ? "Discard" : "Leave"} <kbd>↵</kbd></button>
        </div>
      </div>
    </div>
  {/if}
</div>

<style>
  .editor {
    position: absolute;
    inset: 0;
    z-index: 40;
    display: flex;
    flex-direction: column;
    background: var(--bg);
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

  header {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 52px;
    padding: 0 12px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .title {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--muted-fg);
    font-size: 13px;
  }

  .title strong {
    pointer-events: none; /* clicks fall through to the title, which drags the pane */
    color: var(--fg);
    font-weight: 500;
  }

  .spacer {
    flex: 1;
  }

  .body {
    flex: 1;
    min-height: 0;
    display: flex;
  }

  /* ---------- tool rail ---------- */

  .tools {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    width: 52px;
    padding: 10px 0;
    border-right: 1px solid var(--border);
    flex-shrink: 0;
  }

  .tool {
    display: grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border: none;
    border-radius: 8px;
    background: none;
    color: var(--muted-fg);
    cursor: pointer;
  }

  .tool .icon {
    width: 18px;
    height: 18px;
  }

  .tool:hover {
    background: var(--accent);
    color: var(--fg);
  }

  .tool.on {
    background: var(--primary);
    color: var(--primary-fg);
  }

  .sep {
    width: 24px;
    height: 1px;
    margin: 6px 0;
    background: var(--border);
  }

  /* ---------- canvas ---------- */

  .stage {
    position: relative;
    flex: 1;
    min-width: 0;
    display: grid;
    place-items: center;
    overflow: hidden;
    background:
      radial-gradient(circle, var(--border) 1px, transparent 1px) 0 0 / 16px 16px,
      var(--muted);
  }

  .frame {
    box-shadow: 0 0 0 1px var(--border), 0 8px 30px rgba(0, 0, 0, 0.35);
    line-height: 0;
  }

  .frame.hidden {
    visibility: hidden;
  }

  .note {
    position: absolute;
    color: var(--muted-fg);
  }

  /* ---------- panel ---------- */

  .panel {
    width: 236px;
    flex-shrink: 0;
    padding: 14px;
    border-left: 1px solid var(--border);
    overflow-y: auto;
  }

  h3 {
    margin: 0 0 12px;
    font-size: 13px;
    font-weight: 600;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 14px;
  }

  .field.row {
    flex-direction: row;
    gap: 8px;
    flex-wrap: wrap;
  }

  .label {
    display: flex;
    justify-content: space-between;
    color: var(--muted-fg);
    font-size: 12px;
    font-weight: 500;
  }

  .val {
    color: var(--fg);
    font-variant-numeric: tabular-nums;
  }

  .hint {
    margin: 0 0 12px;
    color: var(--muted-fg);
    font-size: 12px;
    line-height: 1.5;
  }

  .keys {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 10px;
    margin: 0;
    font-size: 12px;
  }

  .keys dt {
    color: var(--muted-fg);
  }

  .keys dd {
    margin: 0;
    text-align: right;
  }

  input[type="range"] {
    width: 100%;
    accent-color: var(--primary);
  }

  .swatches {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .swatch {
    position: relative;
    width: 22px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    cursor: pointer;
  }

  .swatch.on {
    outline: 2px solid var(--ring);
    outline-offset: 2px;
  }

  /* "None": a white square with a red slash. */
  .swatch.none {
    background: linear-gradient(to top right, transparent calc(50% - 1px), #ef4444 calc(50% - 1px), #ef4444 calc(50% + 1px), transparent calc(50% + 1px)), var(--bg);
  }

  .swatch.custom {
    overflow: hidden;
    background: conic-gradient(red, yellow, lime, aqua, blue, magenta, red);
  }

  .swatch.custom input {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    opacity: 0;
    cursor: pointer;
  }

  .seg {
    display: flex;
    border: 1px solid var(--border);
    border-radius: 7px;
    overflow: hidden;
  }

  .seg button {
    flex: 1;
    display: grid;
    place-items: center;
    min-width: 32px;
    height: 30px;
    padding: 0 8px;
    border: none;
    border-right: 1px solid var(--border);
    background: none;
    color: var(--muted-fg);
    font-size: 12.5px;
    cursor: pointer;
    white-space: nowrap;
  }

  .seg button:last-child {
    border-right: none;
  }

  .seg button:hover {
    background: var(--accent);
    color: var(--fg);
  }

  .seg button.on {
    background: var(--primary);
    color: var(--primary-fg);
  }

  /* ---------- buttons ---------- */

  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    height: 32px;
    padding: 0 12px;
    border: 1px solid transparent;
    border-radius: 7px;
    background: none;
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
  }

  .btn kbd {
    margin-left: 2px;
  }

  .btn.primary {
    background: var(--primary);
    color: var(--primary-fg);
  }

  .btn.primary kbd {
    background: transparent;
    border-color: color-mix(in srgb, var(--primary-fg) 25%, transparent);
    color: color-mix(in srgb, var(--primary-fg) 70%, transparent);
  }

  .btn.outline-top {
    border-color: var(--border);
  }

  .btn.outline-top:hover:not(:disabled) {
    background: var(--accent);
  }

  .btn.ghost:hover {
    background: var(--accent);
  }

  .btn.outline {
    flex: 1;
    border-color: var(--border);
  }

  .btn.outline:hover {
    background: var(--accent);
  }

  .btn.outline.danger {
    color: var(--destructive);
  }

  .btn.destructive {
    background: var(--destructive);
    color: #fff;
  }

  .btn.destructive kbd {
    background: transparent;
    border-color: rgba(255, 255, 255, 0.35);
    color: rgba(255, 255, 255, 0.85);
  }

  .btn.wide {
    width: 100%;
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border: none;
    border-radius: 7px;
    background: none;
    color: var(--muted-fg);
    cursor: pointer;
  }

  .icon-btn:hover:not(:disabled) {
    background: var(--accent);
    color: var(--fg);
  }

  .icon-btn:disabled {
    opacity: 0.35;
    cursor: default;
  }

  /* ---------- toast & dialog ---------- */

  .toast {
    position: absolute;
    left: 50%;
    bottom: 16px;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    gap: 12px;
    max-width: 80%;
    padding: 8px 8px 8px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--popover);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.3);
    font-size: 13px;
  }

  .toast.err {
    border-color: color-mix(in srgb, var(--destructive) 50%, var(--border));
  }

  .toast button {
    height: 26px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: none;
    cursor: pointer;
  }

  .scrim {
    position: absolute;
    inset: 0;
    z-index: 5;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.45);
  }

  .dialog {
    width: 340px;
    padding: 18px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--popover);
    box-shadow: 0 16px 40px rgba(0, 0, 0, 0.4);
  }

  .dialog h3 {
    margin-bottom: 6px;
    font-size: 15px;
  }

  .dialog p {
    margin: 0 0 16px;
    color: var(--muted-fg);
    font-size: 13px;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .actions .btn.outline {
    flex: 0;
  }
</style>
