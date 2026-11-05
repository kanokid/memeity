<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import logo from "./assets/logo.png";

  let { onLoad, busy }: { onLoad: () => void; busy: boolean } = $props();

  type Step = {
    title: string;
    why: string;
    link?: { label: string; url: string };
    todo: string[];
    tip?: string;
  };

  const steps: Step[] = [
    {
      title: "Create a Google Cloud project",
      why: "This copy of memeity was built without its Google sign-in client, so it needs one of your own. Official builds skip this whole setup. Developers: see <b>src-tauri/oauth/HOW-TO.md</b> to build it in instead.",
      link: { label: "Open “New project”", url: "https://console.cloud.google.com/projectcreate" },
      todo: [
        "Sign in with the Google account whose Drive holds your memes.",
        "Name the project <b>memeity</b> (any name works) and click <b>Create</b>.",
        "Wait for it to finish, then make sure <b>memeity</b> is the project selected in the top bar.",
      ],
      tip: "First time in Google Cloud? It may ask you to accept the terms of service. No billing or card is needed.",
    },
    {
      title: "Turn on the Google Drive API",
      why: "This lets the project read and write Drive files at all.",
      link: { label: "Open Google Drive API", url: "https://console.cloud.google.com/apis/library/drive.googleapis.com" },
      todo: ["Check the top bar still says <b>memeity</b>.", "Click <b>Enable</b>."],
    },
    {
      title: "Set up the consent screen",
      why: "This is the “memeity wants to access your Google Drive” screen you'll see when signing in.",
      link: { label: "Open Google Auth Platform", url: "https://console.cloud.google.com/auth/overview" },
      todo: [
        "Click <b>Get started</b>.",
        "App name: <b>memeity</b>. User support email: your email. Click <b>Next</b>.",
        "Audience: <b>External</b>. Click <b>Next</b>.",
        "Contact email: your email. Click <b>Next</b>, agree to the policy, then <b>Create</b>.",
      ],
      tip: "If you see “Google Auth Platform not configured yet”, that's expected. Clicking Get started fixes it.",
    },
    {
      title: "Add yourself as a test user",
      why: "The app stays in “testing” mode, so only accounts on this list can sign in.",
      link: { label: "Open Audience", url: "https://console.cloud.google.com/auth/audience" },
      todo: [
        "Scroll to <b>Test users</b> and click <b>Add users</b>.",
        "Enter the Google address you'll sign in with, then <b>Save</b>.",
      ],
      tip: "Leave the publishing status as <b>Testing</b>. You don't need to publish or verify anything.",
    },
    {
      title: "Create a Desktop app client",
      why: "This gives memeity the ID it uses to start the sign-in.",
      link: { label: "Open “Create client”", url: "https://console.cloud.google.com/auth/clients/create" },
      todo: [
        "Application type: <b>Desktop app</b>.",
        "Name: <b>memeity desktop</b> (anything works). Click <b>Create</b>.",
        "In the dialog that pops up, click <b>Download JSON</b>.",
      ],
      tip: "Missed the download? Open the client from the <b>Clients</b> list and use the download icon next to its secret.",
    },
    {
      title: "Load the client JSON",
      why: "The file is usually in Downloads and named like <b>client_secret_…apps.googleusercontent.com.json</b>.",
      todo: [
        "Click the button below and pick that file.",
        "Next you'll sign in. Google will warn “<b>Google hasn't verified this app</b>”. That's your own project, so click <b>Continue</b>.",
        "Allow access to Google Drive when asked.",
      ],
      tip: "In testing mode, Google signs you out after 7 days, so you'll need to sign in again about weekly.",
    },
  ];

  const STORAGE_KEY = "memeity.setupStep";
  function savedStep() {
    try {
      const n = Number(localStorage.getItem(STORAGE_KEY));
      return Number.isInteger(n) && n >= 0 && n < steps.length ? n : 0;
    } catch {
      return 0;
    }
  }

  let index = $state(savedStep());
  let linkError = $state<string | null>(null);
  const step = $derived(steps[index]);
  const last = $derived(index === steps.length - 1);

  function go(i: number) {
    index = Math.max(0, Math.min(steps.length - 1, i));
    linkError = null;
    try {
      localStorage.setItem(STORAGE_KEY, String(index));
    } catch {
      /* per-device convenience only */
    }
  }

  async function openLink() {
    if (!step.link) return;
    linkError = null;
    try {
      await invoke("open_setup_link", { url: step.link.url });
    } catch (e) {
      linkError = String(e);
    }
  }

  function primary() {
    if (last) onLoad();
    else if (step.link) openLink();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowRight") { e.preventDefault(); go(index + 1); }
    else if (e.key === "ArrowLeft") { e.preventDefault(); go(index - 1); }
    else if (e.key === "Enter") { e.preventDefault(); primary(); }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="setup">
  <nav class="rail" aria-label="Setup steps">
    <p class="brand"><img src={logo} alt="" width="22" height="22" />memeity</p>
    <p class="sub">Developer setup</p>
    <ol>
      {#each steps as s, i}
        <li>
          <button class:active={i === index} class:done={i < index} onclick={() => go(i)}>
            <span class="num">
              {#if i < index}
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20 6 9 17l-5-5" /></svg>
              {:else}
                {i + 1}
              {/if}
            </span>
            <span class="label">{s.title}</span>
          </button>
        </li>
      {/each}
    </ol>
  </nav>

  <section class="step">
    <div class="progress" aria-hidden="true">
      <div class="bar" style:width="{((index + 1) / steps.length) * 100}%"></div>
    </div>
    <p class="count">Step {index + 1} of {steps.length}</p>
    <h2>{step.title}</h2>
    <p class="why">{@html step.why}</p>

    <ol class="todo">
      {#each step.todo as t}<li>{@html t}</li>{/each}
    </ol>

    {#if step.tip}<p class="tip">{@html step.tip}</p>{/if}

    <div class="actions">
      {#if last}
        <button class="btn primary" onclick={onLoad} disabled={busy}>Choose client JSON <kbd>↵</kbd></button>
      {:else if step.link}
        <button class="btn primary" onclick={openLink}>
          {step.link.label}
          <svg class="ext" viewBox="0 0 24 24" aria-hidden="true"><path d="M7 17 17 7M7 7h10v10" /></svg>
          <kbd>↵</kbd>
        </button>
      {/if}
      <span class="spacer"></span>
      <button class="btn ghost" onclick={() => go(index - 1)} disabled={index === 0}>Back <kbd>←</kbd></button>
      {#if !last}<button class="btn outline" onclick={() => go(index + 1)}>Next <kbd>→</kbd></button>{/if}
    </div>
    {#if step.link}
      <p class="url">{step.link.url}</p>
    {/if}
    {#if linkError}<p class="err">{linkError}</p>{/if}
  </section>
</div>

<style>
  .setup {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  /* ---------- step rail ---------- */

  .rail {
    width: 220px;
    flex-shrink: 0;
    padding: 18px 10px;
    border-right: 1px solid var(--border);
    background: var(--bar);
    overflow-y: auto;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 8px 2px;
    font-size: 14px;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .brand img {
    flex-shrink: 0;
  }

  .sub {
    margin: 0 8px 14px;
    font-size: 12px;
    color: var(--muted-fg);
  }

  .rail ol {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .rail button {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 7px 8px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--muted-fg);
    font-size: 12.5px;
    text-align: left;
    cursor: pointer;
  }

  .rail button:hover {
    color: var(--fg);
  }

  .rail button.active {
    background: var(--accent);
    color: var(--fg);
    font-weight: 500;
  }

  .num {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    flex-shrink: 0;
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    font: 500 10.5px/1 "Geist Mono Variable", ui-monospace, monospace;
  }

  .active .num {
    border-color: var(--primary);
    background: var(--primary);
    color: var(--primary-fg);
  }

  .done .num {
    border-color: var(--border-strong);
    color: var(--fg);
  }

  .num svg {
    width: 12px;
    height: 12px;
    fill: none;
    stroke: currentColor;
    stroke-width: 3;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .label {
    line-height: 1.3;
  }

  /* ---------- step body ---------- */

  .step {
    flex: 1;
    min-width: 0;
    padding: 0 28px 20px;
    overflow-y: auto;
    color: var(--muted-fg);
  }

  .progress {
    height: 2px;
    margin: 0 -28px 22px;
    background: var(--border);
  }

  .bar {
    height: 100%;
    background: var(--primary);
    transition: width 200ms ease;
  }

  .count {
    margin: 0 0 6px;
    font: 500 11px/1 "Geist Mono Variable", ui-monospace, monospace;
    letter-spacing: 0.02em;
  }

  h2 {
    margin: 0 0 6px;
    font-size: 20px;
    font-weight: 600;
    letter-spacing: -0.025em;
    color: var(--fg);
  }

  .why {
    margin: 0 0 16px;
    font-size: 13px;
  }

  .todo {
    margin: 0 0 16px;
    padding: 0;
    list-style: none;
    counter-reset: todo;
    color: var(--fg);
    font-size: 13px;
  }

  .todo li {
    position: relative;
    margin-bottom: 8px;
    padding-left: 26px;
    counter-increment: todo;
  }

  .todo li::before {
    content: counter(todo);
    position: absolute;
    left: 0;
    top: 1px;
    display: grid;
    place-items: center;
    width: 18px;
    height: 18px;
    border-radius: 4px;
    background: var(--muted);
    color: var(--muted-fg);
    font: 500 10.5px/1 "Geist Mono Variable", ui-monospace, monospace;
  }

  .tip {
    margin: 0 0 18px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--muted);
    font-size: 12px;
  }

  .step :global(b) {
    color: var(--fg);
    font-weight: 500;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .spacer {
    flex: 1;
  }

  .btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    height: 34px;
    padding: 0 14px;
    border: 1px solid transparent;
    border-radius: 6px;
    font-size: 13px;
    font-weight: 500;
    white-space: nowrap;
    cursor: pointer;
  }

  .btn.primary {
    background: var(--primary);
    color: var(--primary-fg);
  }

  .btn.primary:hover:not(:disabled) {
    opacity: 0.9;
  }

  .btn.primary kbd {
    border-color: transparent;
    background: color-mix(in srgb, currentColor 15%, transparent);
    color: inherit;
  }

  .btn.outline {
    border-color: var(--border);
    background: var(--bg);
    color: var(--fg);
  }

  .btn.outline:hover,
  .btn.ghost:hover:not(:disabled) {
    background: var(--accent);
    color: var(--fg);
  }

  .btn.ghost {
    background: none;
    color: var(--muted-fg);
  }

  .btn:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .ext {
    width: 14px;
    height: 14px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .url {
    margin: 10px 0 0;
    font: 11px/1.4 "Geist Mono Variable", ui-monospace, monospace;
    word-break: break-all;
    user-select: text;
    -webkit-user-select: text;
  }

  .err {
    margin: 8px 0 0;
    color: var(--destructive);
    font-size: 12px;
  }
</style>
