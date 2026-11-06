// The quick editor's canvas: a Fabric.js scene over the meme, plus a separate drawing
// ("ink") layer so the eraser only ever removes drawing, never text, shapes or stickers.
//
// Coordinates are the image's own pixels: the canvas is zoomed to fit the editor, and
// export renders at zoom 1/zoom, so the saved meme keeps the original's resolution.
//
// Layers, bottom to top: the meme (background) → drawing → objects (text, shapes,
// stickers), in the order the user arranges them.

import {
  ActiveSelection,
  Canvas,
  Ellipse,
  FabricImage,
  FabricObject,
  InteractiveFabricObject,
  Path,
  Rect,
  Textbox,
  util,
  type TPointerEventInfo,
} from "fabric";

/** Shapes are drawn by dragging on the canvas (Shift: square / circle / 45° arrow). */
export type ShapeKind = "rect" | "ellipse" | "arrow";
export type Tool = "select" | "brush" | "eraser" | ShapeKind;
export const isShapeTool = (t: Tool): t is ShapeKind => t === "rect" || t === "ellipse" || t === "arrow";
export type Kind = "text" | "rect" | "ellipse" | "arrow" | "sticker";
export const FONTS = ["Arial", "Impact", "Times New Roman"] as const;
export type Font = (typeof FONTS)[number];

/** The style of the selected object, or the one the next object of that kind gets. */
export type TextStyle = {
  font: Font;
  fill: string;
  outline: string;
  /** Outline thickness, 0-10 (relative to the text size, so it scales with it). */
  outlineWidth: number;
  bold: boolean;
  italic: boolean;
  align: "left" | "center" | "right";
};
export type ShapeStyle = {
  fill: string | null;
  outline: string | null;
  /** On-screen pixels. */
  outlineWidth: number;
};

/** Extra properties saved with each object (undo/redo snapshots). */
const CUSTOM = ["kind"];
/** Undo steps kept (each drawing step keeps a copy of the drawing layer). */
const MAX_HISTORY = 40;
/** Text outline width 10 = this fraction of the font size. */
const OUTLINE_MAX = 0.12;

// Selection handles, shadcn-ish.
Object.assign(InteractiveFabricObject.ownDefaults, {
  transparentCorners: false,
  cornerStyle: "circle",
  cornerSize: 11,
  cornerColor: "#ffffff",
  cornerStrokeColor: "#3b82f6",
  borderColor: "#3b82f6",
  borderScaleFactor: 1.5,
  padding: 4,
});

type Snapshot = { objects: object[]; ink: HTMLCanvasElement };

export type EngineEvents = {
  /** Selection, history or tool changed: the panel should re-read state. */
  onChange: () => void;
};

const kindOf = (o: FabricObject | undefined): Kind | undefined => (o as any)?.kind;

/** The block arrow, pointing right, in a 100×40 box. */
const ARROW_PATH = "M0 14 L66 14 L66 0 L100 20 L66 40 L66 26 L0 26 Z";
/** Drags shorter than this (screen px) count as a click: a default-size shape. */
const CLICK_SLOP = 4;

export class Engine {
  readonly width: number;
  readonly height: number;
  zoom = 1;
  tool: Tool = "select";
  brushColor = "#ff2d2d";
  /** On-screen pixels. */
  brushSize = 8;
  eraserSize = 28;
  textStyle: TextStyle = {
    font: "Arial",
    fill: "#ffffff",
    outline: "#000000",
    outlineWidth: 5,
    bold: false,
    italic: false,
    align: "center",
  };
  shapeStyle: ShapeStyle = { fill: null, outline: "#ff2d2d", outlineWidth: 5 };
  arrowStyle: ShapeStyle = { fill: "#ff2d2d", outline: null, outlineWidth: 3 };

  private events: EngineEvents;
  private canvas: Canvas;
  private ink: HTMLCanvasElement;
  private inkCtx: CanvasRenderingContext2D;
  private inkObj: FabricImage;
  private history: Snapshot[] = [];
  private at = -1;
  /** The oldest steps were dropped (so even undoing everything leaves changes). */
  private trimmed = false;
  private inkChanged = false;
  private restoring = false;
  /** Undo/redo steps run one at a time (restoring is async). */
  private queue: Promise<void> = Promise.resolve();
  private commitTimer: ReturnType<typeof setTimeout> | undefined;
  private stroke: { x: number; y: number } | null = null;
  /** A shape being drawn by dragging. */
  private creating: { obj: FabricObject; kind: ShapeKind; x: number; y: number } | null = null;
  private textCount = 0;

  constructor(
    el: HTMLCanvasElement,
    background: HTMLImageElement,
    events: EngineEvents,
  ) {
    this.events = events;
    this.width = background.naturalWidth;
    this.height = background.naturalHeight;
    this.canvas = new Canvas(el, {
      width: this.width,
      height: this.height,
      preserveObjectStacking: true,
      selectionColor: "rgba(59, 130, 246, 0.12)",
      selectionBorderColor: "#3b82f6",
      selectionLineWidth: 1,
      stopContextMenu: true,
      fireRightClick: false,
      // Corners stretch freely (text too); Shift keeps the proportions.
      uniformScaling: false,
      uniScaleKey: "shiftKey",
    });
    this.canvas.backgroundImage = new FabricImage(background, { left: 0, top: 0, originX: "left", originY: "top" });

    this.ink = document.createElement("canvas");
    this.ink.width = this.width;
    this.ink.height = this.height;
    this.inkCtx = this.ink.getContext("2d")!;
    this.inkObj = new FabricImage(this.ink, {
      left: 0,
      top: 0,
      originX: "left",
      originY: "top",
      selectable: false,
      evented: false,
      objectCaching: false, // redrawn from the live drawing canvas every frame
    });
    this.canvas.add(this.inkObj);

    const c = this.canvas;
    c.on("mouse:down", (e) => (isShapeTool(this.tool) ? this.shapeStart(e) : this.strokeStart(e)));
    c.on("mouse:move", (e) => (this.creating ? this.shapeMove(e) : this.strokeMove(e)));
    c.on("mouse:up", (e) => (this.creating ? this.shapeEnd(e) : this.strokeEnd()));
    // After fabric has finished the move/resize (a snapshot briefly drops a group selection).
    c.on("object:modified", () => setTimeout(() => this.commit(), 0));
    c.on("text:editing:exited", (e) => {
      // An emptied text box goes away.
      const t = e.target as Textbox;
      if (!t.text?.trim()) c.remove(t);
      this.commit();
    });
    c.on("selection:created", () => events.onChange());
    c.on("selection:updated", () => events.onChange());
    c.on("selection:cleared", () => events.onChange());
    this.snapshot();
  }

  dispose() {
    clearTimeout(this.commitTimer);
    this.canvas.dispose();
  }

  /** Fits the image into a `w`×`h` area (screen pixels). */
  fit(w: number, h: number) {
    const z = Math.max(0.05, Math.min(w / this.width, h / this.height));
    this.zoom = z;
    this.canvas.setDimensions({ width: Math.round(this.width * z), height: Math.round(this.height * z) });
    this.canvas.setZoom(z);
    this.updateCursor();
    this.canvas.requestRenderAll();
  }

  // ---------- tools ----------

  setTool(tool: Tool) {
    this.tool = tool;
    const drawing = tool !== "select";
    if (drawing) this.canvas.discardActiveObject();
    this.canvas.selection = !drawing;
    this.canvas.skipTargetFind = drawing;
    this.updateCursor();
    this.canvas.requestRenderAll();
    this.events.onChange();
  }

  /** A round brush outline as the cursor, sized like the brush on screen. */
  updateCursor() {
    if (this.tool === "select" || isShapeTool(this.tool)) {
      this.canvas.defaultCursor = this.tool === "select" ? "default" : "crosshair";
      this.canvas.setCursor(this.canvas.defaultCursor);
      return;
    }
    const d = Math.max(4, Math.min(126, Math.round(this.tool === "brush" ? this.brushSize : this.eraserSize)));
    const r = d / 2;
    const s = d + 2;
    const svg =
      `<svg xmlns="http://www.w3.org/2000/svg" width="${s}" height="${s}">` +
      `<circle cx="${s / 2}" cy="${s / 2}" r="${r}" fill="none" stroke="black" stroke-width="1.5"/>` +
      `<circle cx="${s / 2}" cy="${s / 2}" r="${r - 0.5}" fill="none" stroke="white" stroke-width="1"/></svg>`;
    this.canvas.defaultCursor = `url("data:image/svg+xml,${encodeURIComponent(svg)}") ${s / 2} ${s / 2}, crosshair`;
    this.canvas.setCursor(this.canvas.defaultCursor);
  }

  // ---------- drawing shapes by dragging ----------

  private shapeStart(e: TPointerEventInfo) {
    const kind = this.tool as ShapeKind;
    const { x, y } = e.scenePoint;
    const obj = this.newShape(kind);
    (obj as any).kind = kind;
    obj.set({ originX: "left", originY: kind === "arrow" ? "center" : "top", left: x, top: y });
    if (kind === "rect") obj.set({ width: 0, height: 0 });
    if (kind === "ellipse") obj.set({ rx: 0, ry: 0 });
    if (kind === "arrow") obj.set({ scaleX: 0.001, scaleY: 0.001 });
    this.creating = { obj, kind, x, y };
    this.canvas.add(obj);
  }

  private shapeMove(e: TPointerEventInfo) {
    const c = this.creating!;
    const p = e.scenePoint;
    let w = p.x - c.x;
    let h = p.y - c.y;
    const shift = (e.e as MouseEvent).shiftKey;
    if (c.kind === "arrow") {
      // From where you pressed to the pointer; Shift snaps to 45° steps.
      let angle = Math.atan2(h, w);
      if (shift) angle = Math.round(angle / (Math.PI / 4)) * (Math.PI / 4);
      const length = Math.hypot(w, h);
      const thickness = Math.min(length, this.defaultSide() * 1.3) / 100;
      c.obj.set({ scaleX: Math.max(length, 1) / 100, scaleY: Math.max(thickness, 0.001), angle: (angle * 180) / Math.PI });
    } else {
      if (shift) {
        // A square / circle: the longer side wins, in the direction you're dragging.
        const side = Math.max(Math.abs(w), Math.abs(h));
        w = Math.sign(w || 1) * side;
        h = Math.sign(h || 1) * side;
      }
      const left = Math.min(c.x, c.x + w);
      const top = Math.min(c.y, c.y + h);
      if (c.kind === "rect") c.obj.set({ left, top, width: Math.abs(w), height: Math.abs(h) });
      else c.obj.set({ left, top, rx: Math.abs(w) / 2, ry: Math.abs(h) / 2 });
    }
    c.obj.setCoords();
    this.canvas.requestRenderAll();
  }

  private shapeEnd(e: TPointerEventInfo) {
    const c = this.creating!;
    this.creating = null;
    const p = e.scenePoint;
    const dragged = Math.hypot(p.x - c.x, p.y - c.y) * this.zoom >= CLICK_SLOP;
    if (!dragged) {
      // Just a click: a default-size shape centred there.
      this.canvas.remove(c.obj);
      this.addShape(c.kind, { x: c.x, y: c.y });
      return;
    }
    this.setTool("select");
    c.obj.setCoords();
    this.canvas.setActiveObject(c.obj);
    this.canvas.requestRenderAll();
    this.commit();
    this.events.onChange();
  }

  private strokeStart(e: TPointerEventInfo) {
    if (this.tool === "select") return;
    const p = e.scenePoint;
    this.stroke = { x: p.x, y: p.y };
    this.paint(p.x, p.y, p.x, p.y);
  }

  private strokeMove(e: TPointerEventInfo) {
    if (!this.stroke) return;
    const p = e.scenePoint;
    this.paint(this.stroke.x, this.stroke.y, p.x, p.y);
    this.stroke = { x: p.x, y: p.y };
  }

  private strokeEnd() {
    if (!this.stroke) return;
    this.stroke = null;
    this.inkChanged = true;
    this.commit();
  }

  private paint(x0: number, y0: number, x1: number, y1: number) {
    const ctx = this.inkCtx;
    const erasing = this.tool === "eraser";
    ctx.save();
    ctx.globalCompositeOperation = erasing ? "destination-out" : "source-over";
    ctx.strokeStyle = this.brushColor;
    ctx.lineWidth = (erasing ? this.eraserSize : this.brushSize) / this.zoom;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.beginPath();
    ctx.moveTo(x0, y0);
    ctx.lineTo(x1 + (x0 === x1 && y0 === y1 ? 0.01 : 0), y1); // a click still leaves a dot
    ctx.stroke();
    ctx.restore();
    this.canvas.requestRenderAll();
  }

  /** Clears the whole drawing layer (objects stay). */
  clearDrawing() {
    this.inkCtx.clearRect(0, 0, this.width, this.height);
    this.inkChanged = true;
    this.canvas.requestRenderAll();
    this.commit();
  }

  // ---------- adding objects ----------

  private place(obj: FabricObject, kind: Kind, top = this.height / 2, left = this.width / 2) {
    (obj as any).kind = kind;
    obj.set({ left, top });
    if (this.tool !== "select") this.setTool("select");
    this.canvas.add(obj);
    this.canvas.setActiveObject(obj);
    this.canvas.requestRenderAll();
    this.commit();
    this.events.onChange();
  }

  addText() {
    const size = Math.round(Math.min(this.width, this.height * 1.4) / 9);
    const t = new Textbox("TEXT", {
      width: this.width * 0.9,
      fontSize: size,
      lineHeight: 1.05,
      paintFirst: "stroke",
      strokeLineJoin: "round",
      splitByGrapheme: false,
    });
    this.styleText(t, this.textStyle);
    // Classic meme layout: the first caption at the top, the second at the bottom.
    const n = this.textCount++;
    const top = n === 0 ? size * 0.75 + this.height * 0.02 : n === 1 ? this.height - size * 0.75 - this.height * 0.02 : this.height / 2;
    this.place(t, "text", top);
    t.enterEditing();
    t.selectAll();
  }

  private styleText(t: Textbox, s: TextStyle) {
    t.set({
      fontFamily: s.font,
      fill: s.fill,
      stroke: s.outlineWidth > 0 ? s.outline : null,
      strokeWidth: (s.outlineWidth / 10) * OUTLINE_MAX * (t.fontSize ?? 40),
      fontWeight: s.bold ? "bold" : "normal",
      fontStyle: s.italic ? "italic" : "normal",
      textAlign: s.align,
    });
    t.initDimensions();
  }

  private styleShape(o: FabricObject, s: ShapeStyle) {
    o.set({
      fill: s.fill ?? "transparent",
      stroke: s.outline,
      strokeWidth: s.outline ? s.outlineWidth / this.zoom : 0,
      strokeUniform: true,
      strokeLineJoin: "round",
    });
  }

  private defaultSide() {
    return Math.min(this.width, this.height) * 0.35;
  }

  /** A shape in the current style (size and position set by the caller). */
  private newShape(kind: ShapeKind): FabricObject {
    // The arrow is a block arrow pointing right; stretch, rotate and recolour it like any shape.
    const o = kind === "rect" ? new Rect() : kind === "ellipse" ? new Ellipse() : new Path(ARROW_PATH);
    this.styleShape(o, kind === "arrow" ? this.arrowStyle : this.shapeStyle);
    return o;
  }

  /** A default-size shape, centred on `at` (or the middle of the meme). */
  addShape(kind: ShapeKind, at?: { x: number; y: number }) {
    const side = this.defaultSide();
    const o = this.newShape(kind);
    if (kind === "rect") o.set({ width: side * 1.3, height: side });
    else if (kind === "ellipse") o.set({ rx: side / 2, ry: side / 2 });
    else o.scale((side * 1.3) / 100);
    this.place(o, kind, at?.y, at?.x);
  }

  /** Adds an image (an object URL, data URL...) as a sticker. */
  async addSticker(url: string) {
    const img = await FabricImage.fromURL(url);
    const fit = (Math.min(this.width, this.height) * 0.4) / Math.max(img.width, img.height);
    img.scale(Math.min(fit, 1 / this.zoom)); // never bigger than it is on screen at 100%
    this.place(img, "sticker");
  }

  // ---------- selection ----------

  /** The selected objects (several for a drag-selection). */
  selected(): FabricObject[] {
    return this.canvas.getActiveObjects();
  }

  /** The kind shared by everything selected, if they're all the same. */
  selectedKind(): Kind | undefined {
    const kinds = new Set(this.selected().map(kindOf));
    return kinds.size === 1 ? [...kinds][0] : undefined;
  }

  isTyping() {
    const o = this.canvas.getActiveObject();
    return o instanceof Textbox && o.isEditing;
  }

  /** Reads the style of the selected object into the panel's defaults. */
  readSelectedStyle() {
    const o = this.selected()[0];
    const kind = kindOf(o);
    if (o instanceof Textbox && kind === "text") {
      this.textStyle = {
        font: (FONTS as readonly string[]).includes(o.fontFamily ?? "") ? (o.fontFamily as Font) : "Arial",
        fill: String(o.fill ?? "#ffffff"),
        outline: o.stroke ? String(o.stroke) : this.textStyle.outline,
        outlineWidth: o.stroke ? Math.round((o.strokeWidth / ((o.fontSize ?? 40) * OUTLINE_MAX)) * 10) : 0,
        bold: o.fontWeight === "bold",
        italic: o.fontStyle === "italic",
        align: (o.textAlign as TextStyle["align"]) ?? "center",
      };
    } else if (kind === "rect" || kind === "ellipse" || kind === "arrow") {
      const s: ShapeStyle = {
        fill: o.fill && o.fill !== "transparent" ? String(o.fill) : null,
        outline: o.stroke && o.strokeWidth > 0 ? String(o.stroke) : null,
        outlineWidth: o.stroke && o.strokeWidth > 0 ? Math.max(1, Math.round(o.strokeWidth * this.zoom)) : (kind === "arrow" ? this.arrowStyle : this.shapeStyle).outlineWidth,
      };
      if (kind === "arrow") this.arrowStyle = s;
      else this.shapeStyle = s;
    }
  }

  /** Applies a text style change to the selected text (and future text). */
  setTextStyle(patch: Partial<TextStyle>) {
    this.textStyle = { ...this.textStyle, ...patch };
    for (const o of this.selected()) if (o instanceof Textbox) this.styleText(o, this.textStyle);
    this.changed();
  }

  /** Text size, in on-screen pixels, of the first selected text. */
  textSize(): number | null {
    const o = this.selected().find((o) => o instanceof Textbox) as Textbox | undefined;
    return o ? Math.round((o.fontSize ?? 40) * (o.scaleY ?? 1) * this.zoom) : null;
  }

  setTextSize(px: number) {
    for (const o of this.selected()) {
      if (!(o instanceof Textbox)) continue;
      o.set({ fontSize: Math.max(4, px / this.zoom / (o.scaleY ?? 1)) });
      this.styleText(o, this.textStyle); // outline follows the size
    }
    this.changed();
  }

  setShapeStyle(kind: "shape" | "arrow", patch: Partial<ShapeStyle>) {
    if (kind === "arrow") this.arrowStyle = { ...this.arrowStyle, ...patch };
    else this.shapeStyle = { ...this.shapeStyle, ...patch };
    for (const o of this.selected()) {
      const k = kindOf(o);
      if (k === "arrow" && kind === "arrow") this.styleShape(o, this.arrowStyle);
      if ((k === "rect" || k === "ellipse") && kind === "shape") this.styleShape(o, this.shapeStyle);
    }
    this.changed();
  }

  /** A style tweak: render now, record an undo step once the user pauses. */
  private changed() {
    this.canvas.requestRenderAll();
    this.events.onChange();
    clearTimeout(this.commitTimer);
    this.commitTimer = setTimeout(() => this.commit(), 350);
  }

  deleteSelected() {
    const objs = this.selected();
    if (!objs.length || this.isTyping()) return;
    this.canvas.discardActiveObject();
    this.canvas.remove(...objs);
    this.canvas.requestRenderAll();
    this.commit();
  }

  async duplicateSelected() {
    const objs = this.selected();
    if (!objs.length || this.isTyping()) return;
    this.canvas.discardActiveObject(); // back to page coordinates before cloning
    const offset = 24 / this.zoom;
    const copies: FabricObject[] = [];
    for (const o of objs) {
      const c = await o.clone(CUSTOM);
      c.set({ left: (c.left ?? 0) + offset, top: (c.top ?? 0) + offset });
      this.canvas.add(c);
      copies.push(c);
    }
    this.select(copies);
    this.commit();
  }

  private select(objs: FabricObject[]) {
    if (objs.length === 1) this.canvas.setActiveObject(objs[0]);
    else if (objs.length > 1) this.canvas.setActiveObject(new ActiveSelection(objs, { canvas: this.canvas }));
    this.canvas.requestRenderAll();
    this.events.onChange();
  }

  selectAll() {
    if (this.tool !== "select") this.setTool("select");
    this.select(this.objects());
  }

  /** Esc: stop typing, else deselect, else back to the select tool. Returns false if there was nothing to do. */
  escape(): boolean {
    const o = this.canvas.getActiveObject();
    if (o instanceof Textbox && o.isEditing) {
      o.exitEditing();
    } else if (o) {
      this.canvas.discardActiveObject();
    } else if (this.tool !== "select") {
      this.setTool("select");
      return true;
    } else {
      return false;
    }
    this.canvas.requestRenderAll();
    this.events.onChange();
    return true;
  }

  /** Arrow keys: move the selection by `dx`, `dy` screen pixels. */
  nudge(dx: number, dy: number) {
    const o = this.canvas.getActiveObject();
    if (!o || this.isTyping()) return;
    o.set({ left: (o.left ?? 0) + dx / this.zoom, top: (o.top ?? 0) + dy / this.zoom });
    o.setCoords();
    this.changed();
  }

  // ---------- arranging (the drawing layer always stays at the bottom) ----------

  arrange(how: "front" | "forward" | "backward" | "back") {
    const objs = this.selected();
    if (!objs.length) return;
    const c = this.canvas;
    const ordered = [...objs].sort((a, b) => c.getObjects().indexOf(a) - c.getObjects().indexOf(b));
    if (how === "front") ordered.forEach((o) => c.bringObjectToFront(o));
    if (how === "back") [...ordered].reverse().forEach((o) => c.sendObjectToBack(o));
    if (how === "forward") [...ordered].reverse().forEach((o) => c.bringObjectForward(o, true));
    if (how === "backward") ordered.forEach((o) => c.sendObjectBackwards(o, true));
    c.sendObjectToBack(this.inkObj);
    c.requestRenderAll();
    this.commit();
  }

  // ---------- history ----------

  private objects(): FabricObject[] {
    return this.canvas.getObjects().filter((o) => o !== this.inkObj);
  }

  private snapshot() {
    let ink = this.history[this.at]?.ink;
    if (!ink || this.inkChanged) {
      ink = document.createElement("canvas");
      ink.width = this.width;
      ink.height = this.height;
      ink.getContext("2d")!.drawImage(this.ink, 0, 0);
      this.inkChanged = false;
    }
    // Serialized while nothing is selected, so objects in a selection save in page coordinates.
    const active = this.canvas.getActiveObjects();
    const group = active.length > 1;
    if (group) this.canvas.discardActiveObject();
    const objects = this.objects().map((o) => o.toObject(CUSTOM));
    if (group) this.select(active);

    // Nothing actually changed (e.g. clicked into a text and out again): no new step.
    const prev = this.history[this.at];
    if (prev && prev.ink === ink && JSON.stringify(prev.objects) === JSON.stringify(objects)) return;

    this.history = this.history.slice(0, this.at + 1);
    this.history.push({ objects, ink });
    if (this.history.length > MAX_HISTORY) {
      this.history.shift();
      this.trimmed = true;
    }
    this.at = this.history.length - 1;
  }

  /** Records an undo step for whatever just changed. */
  commit() {
    clearTimeout(this.commitTimer);
    this.commitTimer = undefined;
    if (this.restoring) return;
    this.snapshot();
    this.events.onChange();
  }

  canUndo() {
    return this.at > 0;
  }
  canRedo() {
    return this.at < this.history.length - 1;
  }
  /** Anything to lose on Cancel? */
  dirty() {
    return this.at > 0 || this.trimmed;
  }

  undo() {
    if (this.isTyping()) return;
    if (this.commitTimer) this.commit(); // a pending style tweak counts as a step first
    if (!this.canUndo()) return;
    const i = --this.at;
    this.queue = this.queue.then(() => this.restore(i));
  }

  redo() {
    if (this.isTyping() || !this.canRedo()) return;
    const i = ++this.at;
    this.queue = this.queue.then(() => this.restore(i));
  }

  private async restore(i: number) {
    const snap = this.history[i];
    if (!snap) return;
    this.restoring = true;
    try {
      this.canvas.discardActiveObject();
      const objects = await util.enlivenObjects<FabricObject>(snap.objects);
      this.canvas.remove(...this.objects());
      this.canvas.add(...objects);
      this.inkCtx.clearRect(0, 0, this.width, this.height);
      this.inkCtx.drawImage(snap.ink, 0, 0);
      this.canvas.requestRenderAll();
    } catch (e) {
      console.error("undo failed", e);
    } finally {
      this.restoring = false;
    }
    this.events.onChange();
  }

  // ---------- export ----------

  /** The finished meme at the original's resolution. */
  async export(type: "image/png" | "image/jpeg"): Promise<Uint8Array> {
    const active = this.canvas.getActiveObject();
    if (active instanceof Textbox && active.isEditing) active.exitEditing();
    this.canvas.discardActiveObject();
    this.canvas.renderAll();
    const el = this.canvas.toCanvasElement(1 / this.zoom);
    // Exactly the original's size (rounding in the zoom can be off by a pixel).
    let out = el;
    if (el.width !== this.width || el.height !== this.height) {
      out = document.createElement("canvas");
      out.width = this.width;
      out.height = this.height;
      out.getContext("2d")!.drawImage(el, 0, 0, this.width, this.height);
    }
    const blob = await new Promise<Blob | null>((resolve) => out.toBlob(resolve, type, 0.92));
    if (!blob) throw new Error("Couldn't export the image.");
    return new Uint8Array(await blob.arrayBuffer());
  }
}
