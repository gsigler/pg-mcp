<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  let copied = $state(null);
  let binaryPath = $state("");
  let status = $state({ claudeDesktop: false, claudeCode: false });
  let message = $state(null);

  let claudeDesktopConfig = $derived(`{
  "mcpServers": {
    "postgres": {
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`);

  let claudeCodeConfig = $derived(`claude mcp add postgres -- ${binaryPath} serve`);

  let cursorConfig = $derived(`{
  "mcpServers": {
    "postgres": {
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`);

  async function load() {
    try {
      binaryPath = await invoke("get_binary_path");
      status = await invoke("check_agent_status");
    } catch (e) {
      binaryPath = "/Applications/pg-mcp.app/Contents/MacOS/pg-mcp";
    }
  }

  async function addToDesktop() {
    try {
      message = { type: "success", text: await invoke("add_to_claude_desktop") };
      status = await invoke("check_agent_status");
    } catch (e) {
      message = { type: "error", text: String(e) };
    }
    setTimeout(() => (message = null), 4000);
  }

  async function addToCode() {
    try {
      message = { type: "success", text: await invoke("add_to_claude_code") };
      status = await invoke("check_agent_status");
    } catch (e) {
      message = { type: "error", text: String(e) };
    }
    setTimeout(() => (message = null), 4000);
  }

  async function copyToClipboard(text, id) {
    try {
      await navigator.clipboard.writeText(text);
      copied = id;
      setTimeout(() => (copied = null), 2000);
    } catch (e) {
      console.error("Copy failed:", e);
    }
  }

  onMount(load);
</script>

<section class="agent-setup">
  <div class="agent-setup-header">
    <span class="agent-setup-title">Agent Setup</span>
  </div>

  {#if message}
    <div class="setup-message" class:success={message.type === "success"} class:error={message.type === "error"}>
      {message.text}
    </div>
  {/if}

  <div class="agent-cards">
    <div class="agent-card">
      <div class="agent-card-info">
        <span class="agent-name">Claude Desktop</span>
        {#if status.claudeDesktop}
          <span class="badge-active">Connected</span>
        {:else}
          <span class="badge-muted">Not connected</span>
        {/if}
      </div>
      {#if status.claudeDesktop}
        <button class="btn btn-small" disabled>Added</button>
      {:else}
        <button class="btn btn-small btn-primary" onclick={addToDesktop}>Add</button>
      {/if}
    </div>

    <div class="agent-card">
      <div class="agent-card-info">
        <span class="agent-name">Claude Code</span>
        {#if status.claudeCode}
          <span class="badge-active">Connected</span>
        {:else}
          <span class="badge-muted">Not connected</span>
        {/if}
      </div>
      {#if status.claudeCode}
        <button class="btn btn-small" disabled>Added</button>
      {:else}
        <button class="btn btn-small btn-primary" onclick={addToCode}>Add</button>
      {/if}
    </div>
  </div>

  <details class="manual-setup">
    <summary>Manual setup / other clients</summary>
    <div class="manual-setup-content">
      <div class="snippet-block">
        <div class="snippet-header">
          <span>Claude Desktop</span>
          <button class="btn btn-small" onclick={() => copyToClipboard(claudeDesktopConfig, "desktop")}>
            {copied === "desktop" ? "Copied" : "Copy"}
          </button>
        </div>
        <pre><code>{claudeDesktopConfig}</code></pre>
      </div>

      <div class="snippet-block">
        <div class="snippet-header">
          <span>Claude Code CLI</span>
          <button class="btn btn-small" onclick={() => copyToClipboard(claudeCodeConfig, "code")}>
            {copied === "code" ? "Copied" : "Copy"}
          </button>
        </div>
        <pre><code>{claudeCodeConfig}</code></pre>
      </div>

      <div class="snippet-block">
        <div class="snippet-header">
          <span>Cursor / Other clients</span>
          <button class="btn btn-small" onclick={() => copyToClipboard(cursorConfig, "cursor")}>
            {copied === "cursor" ? "Copied" : "Copy"}
          </button>
        </div>
        <pre><code>{cursorConfig}</code></pre>
      </div>
    </div>
  </details>
</section>
