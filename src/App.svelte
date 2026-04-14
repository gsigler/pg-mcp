<script>
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import ConnectionList from "./lib/ConnectionList.svelte";
  import ConnectionModal from "./lib/ConnectionModal.svelte";
  import AgentSetup from "./lib/AgentSetup.svelte";

  let config = $state(null);
  let showModal = $state(false);
  let editingConnection = $state(null);
  let showAgentSetup = $state(false);
  let testResults = $state({});

  async function loadConfig() {
    config = await invoke("get_config");
  }

  async function handleSave(connection) {
    config = await invoke("save_connection", { connection });
    showModal = false;
    editingConnection = null;
  }

  async function handleDelete(name) {
    config = await invoke("delete_connection", { name });
  }

  async function handleActivate(name) {
    config = await invoke("set_active", { name });
  }

  async function handleTest(name) {
    testResults = { ...testResults, [name]: { loading: true } };
    try {
      const result = await invoke("test_connection_cmd", { name });
      testResults = { ...testResults, [name]: { success: true, message: result } };
    } catch (e) {
      testResults = { ...testResults, [name]: { success: false, message: String(e) } };
    }
  }

  function handleEdit(name) {
    editingConnection = name;
    showModal = true;
  }

  function handleAdd() {
    editingConnection = null;
    showModal = true;
  }

  onMount(loadConfig);
</script>

<main>
  <header>
    <h1>pg-mcp</h1>
    <div class="header-actions">
      <button class="icon-btn" onclick={() => (showAgentSetup = !showAgentSetup)} title="Agent setup">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <circle cx="12" cy="12" r="3"/>
          <path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09a1.65 1.65 0 00-1-1.51 1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09a1.65 1.65 0 001.51-1 1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/>
        </svg>
      </button>
      <button class="btn btn-primary btn-small" onclick={handleAdd}>+ New</button>
    </div>
  </header>

  {#if config}
    {#if showAgentSetup}
      <AgentSetup />
    {/if}

    <ConnectionList
      {config}
      {testResults}
      onActivate={handleActivate}
      onEdit={handleEdit}
      onDelete={handleDelete}
      onTest={handleTest}
      onAdd={handleAdd}
    />
  {:else}
    <div class="loading">Loading…</div>
  {/if}

  {#if showModal}
    <ConnectionModal
      connectionName={editingConnection}
      onSave={handleSave}
      onCancel={() => { showModal = false; editingConnection = null; }}
      onTest={handleTest}
      testResult={editingConnection ? testResults[editingConnection] : null}
    />
  {/if}
</main>
