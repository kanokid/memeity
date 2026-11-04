<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

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
      why: "memeity talks to Google Drive through its own little Google Cloud project. It's free and only you use it.",
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
  <ol class="rail" aria-label="Setup steps">
    {#each steps as s, i}
      <li>
        <button class:active={i === index} class:done={i < index} onclick={() => go(i)}>
          <span class="dot">{i < index ? "✓" : i + 1}</span>
          <span class="label">{s.title}</span>
        </button>
      </li>
    {/each}
  </ol>

  <section class="step">
    <p class="count">Step {index + 1} of {steps.length} · Connect memeity to Google Drive</p>
    <h2>{step.title}</h2>
    <p class="why">{@html step.why}</p>

    <ol class="todo">
      {#each step.todo as t}<li>{@html t}</li>{/each}
    </ol>

    {#if step.tip}<p class="tip">{@html step.tip}</p>{/if}

    <div class="actions">
      {#if last}
        <button class="primary" onclick={onLoad} disabled={busy}>Choose client JSON <kbd>↵</kbd></button>
      {:else if step.link}
        <button class="primary" onclick={openLink}>{step.link.label} ↗ <kbd>↵</kbd></button>
      {/if}
      <span class="spacer"></span>
      <button onclick={() => go(index - 1)} disabled={index === 0}>Back <kbd>←</kbd></button>
      {#if !last}<button onclick={() => go(index + 1)}>Next <kbd>→</kbd></button>{/if}
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

  .rail {
    list-style: none;
    margin: 0;
    padding: 14px 8px;
    width: 210px;
    flex-shrink: 0;
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }

  .rail button {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 7px 8px;
    border: none;
    border-radius: 8px;
    background: none;
    color: var(--muted);
    font-size: 12px;
    text-align: left;
    cursor: pointer;
  }

  .rail button.active {
    background: var(--accent-soft);
    color: var(--text);
  }

  .dot {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    flex-shrink: 0;
    border-radius: 50%;
    border: 1px solid var(--border);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }

  .active .dot {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }

  .done .dot {
    color: var(--text);
  }

  .label {
    line-height: 1.3;
  }

  .step {
    flex: 1;
    min-width: 0;
    padding: 18px 22px;
    overflow-y: auto;
    color: var(--muted);
  }

  .count {
    margin: 0 0 4px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  h2 {
    margin: 0 0 6px;
    font-size: 18px;
    font-weight: 600;
    color: var(--text);
  }

  .why {
    margin: 0 0 14px;
  }

  .todo {
    margin: 0 0 14px;
    padding-left: 20px;
    color: var(--text);
  }

  .todo li {
    margin-bottom: 7px;
  }

  .tip {
    margin: 0 0 16px;
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--tile);
    font-size: 12px;
  }

  .step :global(b) {
    color: var(--text);
    font-weight: 600;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .spacer {
    flex: 1;
  }

  .actions button {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 7px 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-raised);
    color: var(--text);
    cursor: pointer;
    white-space: nowrap;
  }

  .actions button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }

  .actions button:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .url {
    margin: 8px 0 0;
    font-size: 11px;
    font-family: ui-monospace, "Cascadia Mono", Consolas, monospace;
    word-break: break-all;
    user-select: text;
    -webkit-user-select: text;
  }

  .err {
    margin: 8px 0 0;
    color: var(--danger);
    font-size: 12px;
  }

  kbd {
    font: inherit;
    font-size: 11px;
    padding: 0 5px;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--tile);
    color: var(--text);
  }

  .primary kbd {
    background: rgba(255, 255, 255, 0.2);
    border-color: transparent;
    color: #fff;
  }
</style>
