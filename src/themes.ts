/**
 * Built-in colour themes. A theme sets the CSS variables in app.css (on <html>), so
 * both windows (and the editor) follow it. The choice is per device, kept in the
 * backend config and handed out with `get_status`.
 *
 * Each variant is one palette (a theme's light/dark or flavour). The accent is one of
 * the palette's named colours: it colours primary buttons, the selection ring and
 * focus. Tags (tags.rs COLORS) take the palette's matching colours where it has them.
 */

export type TagColor = "blue" | "green" | "yellow" | "orange" | "red" | "pink" | "purple" | "teal" | "gray";

type Palette = {
  dark: boolean;
  bg: string;
  /** Footer / sidebar / popovers: a step away from `bg`. */
  bar: string;
  /** Quiet fills: key caps, tile placeholders, segmented controls. */
  muted: string;
  /** Hairlines, and hover fills. */
  border: string;
  borderStrong: string;
  fg: string;
  mutedFg: string;
  /** Named colours offered as accents, in the theme's own words. */
  colors: Record<string, string>;
  /** Tag colour -> a name in `colors` (or a hex). Missing ones use memeity's own. */
  tags: Partial<Record<TagColor, string>>;
};

export type Variant = { id: string; name: string; palette: Palette };

export type Theme = {
  id: string;
  name: string;
  /** What the variant picker is called ("Flavour", "Brightness"...). */
  variantLabel?: string;
  variants: Variant[];
  defaultVariant: string;
  /** null = monochrome (no accent colour). */
  defaultAccent: string | null;
  /** Offers "Mono" (no accent) as well as the palette's colours. */
  mono?: boolean;
};

export type ThemeChoice = { theme: string; variant: string | null; accent: string | null };

// ---------- palettes ----------

const zincDark: Palette = {
  dark: true,
  bg: "#09090b",
  bar: "#0c0c0e",
  muted: "#18181b",
  border: "#27272a",
  borderStrong: "#3f3f46",
  fg: "#fafafa",
  mutedFg: "#a1a1aa",
  colors: { blue: "#60a5fa", green: "#4ade80", yellow: "#facc15", orange: "#fb923c", red: "#f87171", pink: "#f472b6", purple: "#c084fc", teal: "#2dd4bf" },
  tags: {},
};

const zincLight: Palette = {
  dark: false,
  bg: "#ffffff",
  bar: "#fafafa",
  muted: "#f4f4f5",
  border: "#e4e4e7",
  borderStrong: "#d4d4d8",
  fg: "#09090b",
  mutedFg: "#71717a",
  colors: { blue: "#2563eb", green: "#16a34a", yellow: "#a16207", orange: "#ea580c", red: "#dc2626", pink: "#db2777", purple: "#9333ea", teal: "#0d9488" },
  tags: {},
};

/** Catppuccin's 14 accents, in its own order. */
function catppuccin(dark: boolean, c: Record<string, string>, base: Record<string, string>): Palette {
  return {
    dark,
    bg: base.base,
    bar: base.mantle,
    muted: dark ? base.mantle : base.crust,
    border: base.surface0,
    borderStrong: base.surface1,
    fg: base.text,
    mutedFg: base.subtext0,
    colors: c,
    // Catppuccin's "overlay" grey is a tag colour, not an accent.
    tags: { blue: "blue", green: "green", yellow: "yellow", orange: "peach", red: "red", pink: "pink", purple: "mauve", teal: "teal", gray: base.overlay },
  };
}

const catppuccinVariants: Variant[] = [
  {
    id: "latte",
    name: "Latte",
    palette: catppuccin(
      false,
      { rosewater: "#dc8a78", flamingo: "#dd7878", pink: "#ea76cb", mauve: "#8839ef", red: "#d20f39", maroon: "#e64553", peach: "#fe640b", yellow: "#df8e1d", green: "#40a02b", teal: "#179299", sky: "#04a5e5", sapphire: "#209fb5", blue: "#1e66f5", lavender: "#7287fd" },
      { base: "#eff1f5", mantle: "#e6e9ef", crust: "#dce0e8", surface0: "#ccd0da", surface1: "#bcc0cc", text: "#4c4f69", subtext0: "#6c6f85", overlay: "#8c8fa1" },
    ),
  },
  {
    id: "frappe",
    name: "Frappé",
    palette: catppuccin(
      true,
      { rosewater: "#f2d5cf", flamingo: "#eebebe", pink: "#f4b8e4", mauve: "#ca9ee6", red: "#e78284", maroon: "#ea999c", peach: "#ef9f76", yellow: "#e5c890", green: "#a6d189", teal: "#81c8be", sky: "#99d1db", sapphire: "#85c1dc", blue: "#8caaee", lavender: "#babbf1" },
      { base: "#303446", mantle: "#292c3c", crust: "#232634", surface0: "#414559", surface1: "#51576d", text: "#c6d0f5", subtext0: "#a5adce", overlay: "#838ba7" },
    ),
  },
  {
    id: "macchiato",
    name: "Macchiato",
    palette: catppuccin(
      true,
      { rosewater: "#f4dbd6", flamingo: "#f0c6c6", pink: "#f5bde6", mauve: "#c6a0f6", red: "#ed8796", maroon: "#ee99a0", peach: "#f5a97f", yellow: "#eed49f", green: "#a6da95", teal: "#8bd5ca", sky: "#91d7e3", sapphire: "#7dc4e4", blue: "#8aadf4", lavender: "#b7bdf8" },
      { base: "#24273a", mantle: "#1e2030", crust: "#181926", surface0: "#363a4f", surface1: "#494d64", text: "#cad3f5", subtext0: "#a5adcb", overlay: "#8087a2" },
    ),
  },
  {
    id: "mocha",
    name: "Mocha",
    palette: catppuccin(
      true,
      { rosewater: "#f5e0dc", flamingo: "#f2cdcd", pink: "#f5c2e7", mauve: "#cba6f7", red: "#f38ba8", maroon: "#eba0ac", peach: "#fab387", yellow: "#f9e2af", green: "#a6e3a1", teal: "#94e2d5", sky: "#89dceb", sapphire: "#74c7ec", blue: "#89b4fa", lavender: "#b4befe" },
      { base: "#1e1e2e", mantle: "#181825", crust: "#11111b", surface0: "#313244", surface1: "#45475a", text: "#cdd6f4", subtext0: "#a6adc8", overlay: "#7f849c" },
    ),
  },
];
/** Tags named after palette colours: resolved to hex below. */
const sameNames: Partial<Record<TagColor, string>> = {
  blue: "blue",
  green: "green",
  yellow: "yellow",
  orange: "orange",
  red: "red",
  pink: "pink",
  purple: "purple",
  teal: "teal",
};

export const THEMES: Theme[] = [
  {
    id: "default",
    name: "memeity",
    variantLabel: "Appearance",
    variants: [
      { id: "system", name: "System", palette: zincDark }, // resolved at apply time
      { id: "dark", name: "Dark", palette: zincDark },
      { id: "light", name: "Light", palette: zincLight },
    ],
    defaultVariant: "system",
    defaultAccent: null,
    mono: true,
  },
  {
    id: "catppuccin",
    name: "Catppuccin",
    variantLabel: "Flavour",
    variants: catppuccinVariants,
    defaultVariant: "mocha",
    defaultAccent: "mauve",
  },
  {
    id: "dracula",
    name: "Dracula",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#282a36",
          bar: "#21222c",
          muted: "#21222c",
          border: "#44475a",
          borderStrong: "#565a70",
          fg: "#f8f8f2",
          mutedFg: "#9aa3c6",
          colors: { purple: "#bd93f9", pink: "#ff79c6", cyan: "#8be9fd", green: "#50fa7b", yellow: "#f1fa8c", orange: "#ffb86c", red: "#ff5555" },
          tags: { ...sameNames, blue: "cyan", teal: "cyan" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "purple",
  },
  {
    id: "everforest",
    name: "Everforest",
    variantLabel: "Brightness",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#2d353b",
          bar: "#232a2e",
          muted: "#232a2e",
          border: "#3d484d",
          borderStrong: "#475258",
          fg: "#d3c6aa",
          mutedFg: "#9da9a0",
          colors: { green: "#a7c080", aqua: "#83c092", blue: "#7fbbb3", purple: "#d699b6", red: "#e67e80", orange: "#e69875", yellow: "#dbbc7f" },
          tags: { ...sameNames, teal: "aqua", pink: "purple" },
        },
      },
      {
        id: "light",
        name: "Light",
        palette: {
          dark: false,
          bg: "#fdf6e3",
          bar: "#f4f0d9",
          muted: "#efebd4",
          border: "#e6e2cc",
          borderStrong: "#d5d1bc",
          fg: "#5c6a72",
          mutedFg: "#829181",
          colors: { green: "#8da101", aqua: "#35a77c", blue: "#3a94c5", purple: "#df69ba", red: "#f85552", orange: "#f57d26", yellow: "#dfa000" },
          tags: { ...sameNames, teal: "aqua", pink: "purple" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "green",
  },
  {
    id: "gruvbox",
    name: "Gruvbox",
    variantLabel: "Brightness",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#282828",
          bar: "#1d2021",
          muted: "#1d2021",
          border: "#3c3836",
          borderStrong: "#504945",
          fg: "#ebdbb2",
          mutedFg: "#a89984",
          colors: { yellow: "#fabd2f", orange: "#fe8019", red: "#fb4934", green: "#b8bb26", aqua: "#8ec07c", blue: "#83a598", purple: "#d3869b" },
          tags: { ...sameNames, teal: "aqua", pink: "purple" },
        },
      },
      {
        id: "light",
        name: "Light",
        palette: {
          dark: false,
          bg: "#fbf1c7",
          bar: "#f2e5bc",
          muted: "#ebdbb2",
          border: "#ebdbb2",
          borderStrong: "#d5c4a1",
          fg: "#3c3836",
          mutedFg: "#7c6f64",
          colors: { yellow: "#b57614", orange: "#af3a03", red: "#9d0006", green: "#79740e", aqua: "#427b58", blue: "#076678", purple: "#8f3f71" },
          tags: { ...sameNames, teal: "aqua", pink: "purple" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "orange",
  },
  {
    id: "kanagawa",
    name: "Kanagawa",
    variants: [
      {
        id: "wave",
        name: "Wave",
        palette: {
          dark: true,
          bg: "#1f1f28",
          bar: "#16161d",
          muted: "#16161d",
          border: "#2a2a37",
          borderStrong: "#363646",
          fg: "#dcd7ba",
          mutedFg: "#a6a69c",
          colors: { crystal: "#7e9cd8", spring: "#98bb6c", carp: "#e6c384", surimi: "#ffa066", wave: "#e46876", sakura: "#d27e99", violet: "#957fb8", aqua: "#7aa89f" },
          tags: { blue: "crystal", green: "spring", yellow: "carp", orange: "surimi", red: "wave", pink: "sakura", purple: "violet", teal: "aqua" },
        },
      },
    ],
    defaultVariant: "wave",
    defaultAccent: "crystal",
  },
  {
    id: "nord",
    name: "Nord",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#2e3440",
          bar: "#292e39",
          muted: "#292e39",
          border: "#3b4252",
          borderStrong: "#4c566a",
          fg: "#eceff4",
          mutedFg: "#a3abba",
          colors: { frost: "#88c0d0", glacier: "#81a1c1", arctic: "#5e81ac", teal: "#8fbcbb", green: "#a3be8c", yellow: "#ebcb8b", orange: "#d08770", red: "#bf616a", purple: "#b48ead" },
          tags: { blue: "glacier", green: "green", yellow: "yellow", orange: "orange", red: "red", pink: "purple", purple: "purple", teal: "teal" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "frost",
  },
  {
    id: "onedark",
    name: "One Dark",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#282c34",
          bar: "#21252b",
          muted: "#21252b",
          border: "#3e4451",
          borderStrong: "#4b5263",
          fg: "#abb2bf",
          mutedFg: "#7f848e",
          colors: { blue: "#61afef", cyan: "#56b6c2", green: "#98c379", yellow: "#e5c07b", orange: "#d19a66", red: "#e06c75", purple: "#c678dd" },
          tags: { ...sameNames, teal: "cyan", pink: "purple" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "blue",
  },
  {
    id: "rosepine",
    name: "Rosé Pine",
    variantLabel: "Variant",
    variants: [
      {
        id: "main",
        name: "Main",
        palette: {
          dark: true,
          bg: "#191724",
          bar: "#1f1d2e",
          muted: "#1f1d2e",
          border: "#26233a",
          borderStrong: "#403d52",
          fg: "#e0def4",
          mutedFg: "#908caa",
          colors: { rose: "#ebbcba", love: "#eb6f92", gold: "#f6c177", pine: "#31748f", foam: "#9ccfd8", iris: "#c4a7e7" },
          tags: { blue: "pine", teal: "foam", yellow: "gold", red: "love", pink: "rose", purple: "iris" },
        },
      },
      {
        id: "moon",
        name: "Moon",
        palette: {
          dark: true,
          bg: "#232136",
          bar: "#2a273f",
          muted: "#2a273f",
          border: "#393552",
          borderStrong: "#44415a",
          fg: "#e0def4",
          mutedFg: "#908caa",
          colors: { rose: "#ea9a97", love: "#eb6f92", gold: "#f6c177", pine: "#3e8fb0", foam: "#9ccfd8", iris: "#c4a7e7" },
          tags: { blue: "pine", teal: "foam", yellow: "gold", red: "love", pink: "rose", purple: "iris" },
        },
      },
      {
        id: "dawn",
        name: "Dawn",
        palette: {
          dark: false,
          bg: "#faf4ed",
          bar: "#fffaf3",
          muted: "#f2e9e1",
          border: "#f2e9e1",
          borderStrong: "#dfdad9",
          fg: "#575279",
          mutedFg: "#797593",
          colors: { rose: "#d7827e", love: "#b4637a", gold: "#ea9d34", pine: "#286983", foam: "#56949f", iris: "#907aa9" },
          tags: { blue: "pine", teal: "foam", yellow: "gold", red: "love", pink: "rose", purple: "iris" },
        },
      },
    ],
    defaultVariant: "main",
    defaultAccent: "rose",
  },
  {
    id: "solarized",
    name: "Solarized",
    variantLabel: "Brightness",
    variants: [
      {
        id: "dark",
        name: "Dark",
        palette: {
          dark: true,
          bg: "#002b36",
          bar: "#00252f",
          muted: "#073642",
          border: "#073642",
          borderStrong: "#244651",
          fg: "#93a1a1",
          mutedFg: "#839496",
          colors: { blue: "#268bd2", cyan: "#2aa198", green: "#859900", yellow: "#b58900", orange: "#cb4b16", red: "#dc322f", magenta: "#d33682", violet: "#6c71c4" },
          tags: { ...sameNames, teal: "cyan", pink: "magenta", purple: "violet" },
        },
      },
      {
        id: "light",
        name: "Light",
        palette: {
          dark: false,
          bg: "#fdf6e3",
          bar: "#eee8d5",
          muted: "#eee8d5",
          border: "#e6dfca",
          borderStrong: "#d3cbb7",
          fg: "#586e75",
          mutedFg: "#839496",
          colors: { blue: "#268bd2", cyan: "#2aa198", green: "#859900", yellow: "#b58900", orange: "#cb4b16", red: "#dc322f", magenta: "#d33682", violet: "#6c71c4" },
          tags: { ...sameNames, teal: "cyan", pink: "magenta", purple: "violet" },
        },
      },
    ],
    defaultVariant: "dark",
    defaultAccent: "blue",
  },
  {
    id: "tokyonight",
    name: "Tokyo Night",
    variantLabel: "Style",
    variants: [
      {
        id: "night",
        name: "Night",
        palette: {
          dark: true,
          bg: "#1a1b26",
          bar: "#16161e",
          muted: "#16161e",
          border: "#292e42",
          borderStrong: "#3b4261",
          fg: "#c0caf5",
          mutedFg: "#737aa2",
          colors: { blue: "#7aa2f7", cyan: "#7dcfff", magenta: "#bb9af7", green: "#9ece6a", teal: "#1abc9c", yellow: "#e0af68", orange: "#ff9e64", red: "#f7768e" },
          tags: { ...sameNames, pink: "magenta", purple: "magenta" },
        },
      },
      {
        id: "storm",
        name: "Storm",
        palette: {
          dark: true,
          bg: "#24283b",
          bar: "#1f2335",
          muted: "#1f2335",
          border: "#292e42",
          borderStrong: "#3b4261",
          fg: "#c0caf5",
          mutedFg: "#737aa2",
          colors: { blue: "#7aa2f7", cyan: "#7dcfff", magenta: "#bb9af7", green: "#9ece6a", teal: "#1abc9c", yellow: "#e0af68", orange: "#ff9e64", red: "#f7768e" },
          tags: { ...sameNames, pink: "magenta", purple: "magenta" },
        },
      },
      {
        id: "day",
        name: "Day",
        palette: {
          dark: false,
          bg: "#e1e2e7",
          bar: "#d0d5e3",
          muted: "#d0d5e3",
          border: "#c4c8da",
          borderStrong: "#a8aecb",
          fg: "#3760bf",
          mutedFg: "#6172b0",
          colors: { blue: "#2e7de9", cyan: "#007197", magenta: "#9854f1", green: "#587539", teal: "#118c74", yellow: "#8c6c3e", orange: "#b15c00", red: "#f52a65" },
          tags: { ...sameNames, pink: "magenta", purple: "magenta" },
        },
      },
    ],
    defaultVariant: "night",
    defaultAccent: "blue",
  },
];

export const DEFAULT_CHOICE: ThemeChoice = { theme: "default", variant: "system", accent: null };

export const themeById = (id: string | null | undefined) => THEMES.find((t) => t.id === id) ?? THEMES[0];

/** The variant to use (an unknown or missing one falls back to the theme's default). */
export function variantOf(theme: Theme, id: string | null | undefined): Variant {
  return theme.variants.find((v) => v.id === id) ?? theme.variants.find((v) => v.id === theme.defaultVariant)!;
}

/** Accent names the theme offers ("" stands for Mono). */
export function accentsOf(theme: Theme): string[] {
  const names = Object.keys(theme.variants[0].palette.colors);
  return theme.mono ? ["", ...names] : names;
}

/** The accent to use: the chosen one if this theme has it, else the theme's default. */
export function accentOf(theme: Theme, accent: string | null | undefined): string | null {
  if (accent === "" && theme.mono) return null;
  if (accent && accent in theme.variants[0].palette.colors) return accent;
  return theme.defaultAccent;
}

const prefersLight = window.matchMedia("(prefers-color-scheme: light)");

function paletteFor(choice: ThemeChoice): { palette: Palette; accent: string | null } {
  const theme = themeById(choice.theme);
  let variant = variantOf(theme, choice.variant);
  if (theme.id === "default" && variant.id === "system") {
    variant = variantOf(theme, prefersLight.matches ? "light" : "dark");
  }
  return { palette: variant.palette, accent: accentOf(theme, choice.accent) };
}

let current: ThemeChoice = DEFAULT_CHOICE;

/** WCAG relative luminance of a #rrggbb colour. */
function luminance(hex: string) {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

const contrast = (a: string, b: string) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

/** Text for on top of `fill`: whichever of the palette's background/text reads better. */
const textOn = (fill: string, p: Palette) => (contrast(fill, p.bg) >= contrast(fill, p.fg) ? p.bg : p.fg);

/** Remembered locally too, so a window opening paints in the theme before it hears from the backend. */
const STORE_KEY = "theme";

/** Sets the CSS variables for `choice` on <html>. Cheap: call it on every status refresh. */
export function applyTheme(choice: ThemeChoice | null | undefined) {
  current = choice ?? DEFAULT_CHOICE;
  const { palette: p, accent } = paletteFor(current);
  const accentHex = accent ? p.colors[accent] : null;
  const vars: Record<string, string> = {
    "--bg": p.bg,
    "--fg": p.fg,
    "--muted": p.muted,
    "--muted-fg": p.mutedFg,
    "--accent": p.border, // hover fills
    "--border": p.border,
    "--border-strong": p.borderStrong,
    "--popover": p.bg,
    "--bar": p.bar,
    // No accent: monochrome, like shadcn's zinc.
    "--primary": accentHex ?? p.fg,
    "--primary-fg": textOn(accentHex ?? p.fg, p),
    "--ring": accentHex ?? p.fg,
  };
  // Tags and status colours from the palette where it has them, else memeity's own.
  // Every variable is set every time, so nothing lingers from the previous theme.
  const own = p.dark ? zincDark : zincLight;
  const named = (tag: TagColor) => {
    const name = p.tags[tag];
    return name ? (p.colors[name] ?? (name.startsWith("#") ? name : undefined)) : undefined;
  };
  for (const tag of ["blue", "green", "yellow", "orange", "red", "pink", "purple", "teal"] as TagColor[]) {
    vars[`--tag-${tag}`] = named(tag) ?? own.colors[tag];
  }
  vars["--tag-gray"] = named("gray") ?? p.mutedFg;
  vars["--destructive"] = named("red") ?? (p.dark ? "#ef4444" : "#dc2626");
  vars["--success"] = named("green") ?? (p.dark ? "#22c55e" : "#16a34a");
  vars["--warning"] = named("yellow") ?? (p.dark ? "#f59e0b" : "#d97706");
  const root = document.documentElement;
  for (const [k, v] of Object.entries(vars)) root.style.setProperty(k, v);
  root.style.colorScheme = p.dark ? "dark" : "light";
  root.dataset.theme = p.dark ? "dark" : "light";
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(current));
  } catch {
    // private mode etc.: it just paints the default for a moment
  }
}

try {
  const saved = localStorage.getItem(STORE_KEY);
  if (saved) applyTheme(JSON.parse(saved));
} catch {
  // nothing saved, or unreadable: app.css's defaults stand
}

// "System" follows the OS as it switches between light and dark.
prefersLight.addEventListener("change", () => applyTheme(current));

/** A few colours to preview a variant with (background, text, accent, then some more). */
export function swatchesOf(theme: Theme, variant: Variant, accent: string | null): string[] {
  const p = theme.id === "default" && variant.id === "system" ? (prefersLight.matches ? zincLight : zincDark) : variant.palette;
  const a = accentOf(theme, accent);
  const colors = Object.values(p.colors);
  return [p.bg, p.fg, a ? p.colors[a] : p.fg, ...colors.slice(0, 4)];
}
