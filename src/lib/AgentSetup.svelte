<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  let copied = $state(null);
  let binaryPath = $state("");
  let status = $state({
    claudeDesktop: false,
    claudeCode: false,
    cursor: false,
    vscode: false,
    codex: false,
  });
  let message = $state(null);

  // Auto-installable targets. Each command writes to a well-known user-scope
  // config file on macOS and reports back a short status string. The `id`
  // matches the key returned by `check_agent_status` so a single response
  // hydrates every badge.
  const agents = [
    { id: "claudeDesktop", name: "Claude Desktop", command: "add_to_claude_desktop" },
    { id: "claudeCode", name: "Claude Code", command: "add_to_claude_code" },
    { id: "cursor", name: "Cursor", command: "add_to_cursor" },
    { id: "vscode", name: "VS Code", command: "add_to_vscode" },
    { id: "codex", name: "Codex (CLI + IDE + App)", command: "add_to_codex" },
  ];

  // Manual-setup snippets. `path` is where the user should paste; `format`
  // is the language for the code block. Kept separate from the auto-install
  // list because one "install" may map to multiple config surfaces (e.g.
  // Claude Code also has a CLI one-liner).
  let snippets = $derived([
    {
      id: "claudeDesktop",
      label: "Claude Desktop",
      path: "~/Library/Application Support/Claude/claude_desktop_config.json",
      body: `{
  "mcpServers": {
    "postgres": {
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`,
    },
    {
      id: "claudeCodeCli",
      label: "Claude Code (CLI one-liner)",
      path: null,
      body: `claude mcp add postgres -- ${binaryPath} serve`,
    },
    {
      id: "cursor",
      label: "Cursor",
      path: "~/.cursor/mcp.json",
      body: `{
  "mcpServers": {
    "postgres": {
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`,
    },
    {
      id: "vscode",
      label: "VS Code (key is `servers`, not `mcpServers`)",
      path: "~/Library/Application Support/Code/User/mcp.json",
      body: `{
  "servers": {
    "postgres": {
      "type": "stdio",
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`,
    },
    {
      id: "codex",
      label: "Codex — CLI, IDE extension, and desktop app all read this file",
      path: "~/.codex/config.toml",
      body: `[mcp_servers.postgres]
command = "${binaryPath}"
args = ["serve"]`,
    },
    {
      id: "generic",
      label: "Generic MCP client (Zed, Cline, etc. — schemas vary; see the client's docs)",
      path: null,
      body: `{
  "mcpServers": {
    "postgres": {
      "command": "${binaryPath}",
      "args": ["serve"]
    }
  }
}`,
    },
  ]);

  async function load() {
    try {
      binaryPath = await invoke("get_binary_path");
      status = await invoke("check_agent_status");
    } catch (e) {
      binaryPath = "/Applications/pg-mcp.app/Contents/MacOS/pg-mcp";
    }
  }

  async function install(agent) {
    try {
      message = { type: "success", text: await invoke(agent.command) };
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
    <div
      class="setup-message"
      class:success={message.type === "success"}
      class:error={message.type === "error"}
    >
      {message.text}
    </div>
  {/if}

  <div class="agent-cards">
    {#each agents as agent (agent.id)}
      <div class="agent-card">
        <div class="agent-card-info">
          <span class="agent-name">{agent.name}</span>
          {#if status[agent.id]}
            <span class="badge-active">Added</span>
          {:else}
            <span class="badge-muted">Not connected</span>
          {/if}
        </div>
        {#if !status[agent.id]}
          <button type="button" class="btn btn-small btn-primary" onclick={() => install(agent)}>
            Add
          </button>
        {/if}
      </div>
    {/each}
  </div>

  <details class="manual-setup">
    <summary>Manual setup / other clients</summary>
    <div class="manual-setup-content">
      {#each snippets as snippet (snippet.id)}
        <div class="snippet-block">
          <div class="snippet-header">
            <span>{snippet.label}</span>
            <button
              type="button"
              class="btn btn-small"
              onclick={() => copyToClipboard(snippet.body, snippet.id)}
            >
              {copied === snippet.id ? "Copied" : "Copy"}
            </button>
          </div>
          {#if snippet.path}
            <div class="snippet-path">{snippet.path}</div>
          {/if}
          <pre><code>{snippet.body}</code></pre>
        </div>
      {/each}
    </div>
  </details>
</section>
