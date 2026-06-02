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
      <button
        type="button"
        class="btn btn-small"
        class:active={showAgentSetup}
        aria-pressed={showAgentSetup}
        onclick={() => (showAgentSetup = !showAgentSetup)}
      >
        {showAgentSetup ? "Hide agent setup" : "Agent setup"}
      </button>
      <button type="button" class="btn btn-primary btn-small" onclick={handleAdd}>
        New connection
      </button>
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
      testResult={editingConnection ? testResults[editingConnection] : null}
    />
  {/if}
</main>
