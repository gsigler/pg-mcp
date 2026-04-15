<script>
  import { invoke } from "@tauri-apps/api/core";
  import Toggle from "./Toggle.svelte";

  let { connectionName = null, onSave, onCancel, onTest, testResult = null } = $props();

  const COLOR_PALETTE = [
    "#22c55e", // green
    "#3b82f6", // blue
    "#f59e0b", // amber
    "#ef4444", // red
    "#a855f7", // purple
    "#06b6d4", // cyan
    "#f97316", // orange
    "#ec4899", // pink
  ];

  let name = $state("");
  let connectionString = $state("");
  let host = $state("localhost");
  let port = $state(5432);
  let database = $state("");
  let user = $state("postgres");
  let password = $state("");
  let ssl = $state(false);
  let readonly = $state(true);
  let redactPii = $state(false);
  let color = $state(COLOR_PALETTE[0]);
  let localTestResult = $state(null);

  let isEdit = $derived(!!connectionName);

  $effect(() => {
    if (connectionName) {
      loadConnection(connectionName);
    }
  });

  async function loadConnection(connName) {
    try {
      const conn = await invoke("get_connection_for_edit", { name: connName });
      name = conn.name;
      host = conn.host;
      port = conn.port;
      database = conn.database;
      user = conn.user;
      ssl = conn.ssl;
      readonly = conn.readonly;
      redactPii = conn.redactPii ?? false;
      color = conn.color || COLOR_PALETTE[0];
      connectionString = conn.connectionString || "";
    } catch (e) {
      console.error("Failed to load connection:", e);
    }
  }

  function buildConnection() {
    return {
      name: name.trim(),
      host,
      port,
      database,
      user,
      password,
      ssl,
      readonly,
      redactPii,
      color,
      connectionString: connectionString.trim() || null,
    };
  }

  function handleSave() {
    if (!name.trim()) return;
    onSave(buildConnection());
  }

  async function handleTest() {
    if (!name.trim()) return;
    localTestResult = { loading: true };
    try {
      await invoke("save_connection", { connection: buildConnection() });
      const result = await invoke("test_connection_cmd", { name: name.trim() });
      localTestResult = { success: true, message: result };
    } catch (e) {
      localTestResult = { success: false, message: String(e) };
    }
  }

  function handleKeydown(e) {
    if (e.key === "Escape") onCancel();
  }

  let displayTestResult = $derived(localTestResult || testResult);
</script>

<svelte:window onkeydown={handleKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="modal-overlay" onclick={onCancel}>
  <div class="modal" onclick={(e) => e.stopPropagation()}>
    <h2>{isEdit ? "Edit Connection" : "Add Connection"}</h2>

    <div class="form-row name-row">
      <div class="form-group flex-grow">
        <label for="conn-name">Connection Name</label>
        <input
          id="conn-name"
          type="text"
          bind:value={name}
          placeholder="my-database"
          disabled={isEdit}
        />
      </div>
      <div class="form-group">
        <label>Color</label>
        <div class="color-picker">
          {#each COLOR_PALETTE as c}
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <button
              type="button"
              class="color-swatch"
              class:selected={color === c}
              style="background: {c}"
              onclick={() => (color = c)}
              aria-label="Select color {c}"
            ></button>
          {/each}
        </div>
      </div>
    </div>

    <div class="form-group">
      <label for="conn-string">Connection String</label>
      <input
        id="conn-string"
        type="text"
        bind:value={connectionString}
        placeholder="postgresql://user:pass@host:5432/db"
      />
    </div>

    <div class="divider">
      <span>or configure individually</span>
    </div>

    <div class="form-row">
      <div class="form-group flex-grow">
        <label for="conn-host">Host</label>
        <input id="conn-host" type="text" bind:value={host} placeholder="localhost" />
      </div>
      <div class="form-group" style="width: 100px">
        <label for="conn-port">Port</label>
        <input id="conn-port" type="number" bind:value={port} />
      </div>
    </div>

    <div class="form-group">
      <label for="conn-db">Database</label>
      <input id="conn-db" type="text" bind:value={database} placeholder="my_database" />
    </div>

    <div class="form-row">
      <div class="form-group flex-grow">
        <label for="conn-user">User</label>
        <input id="conn-user" type="text" bind:value={user} placeholder="postgres" />
      </div>
      <div class="form-group flex-grow">
        <label for="conn-pass">Password</label>
        <input
          id="conn-pass"
          type="password"
          bind:value={password}
          placeholder={isEdit ? "(keep existing)" : ""}
        />
      </div>
    </div>

    <div class="toggle-row">
      <Toggle label="SSL" bind:checked={ssl} />
      <Toggle label="Read-only" bind:checked={readonly} />
      <Toggle label="Redact PII" bind:checked={redactPii} />
    </div>

    {#if displayTestResult}
      <div
        class="test-result-inline"
        class:success={displayTestResult.success}
        class:error={!displayTestResult.success && !displayTestResult.loading}
        class:loading={displayTestResult.loading}
      >
        {#if displayTestResult.loading}
          Testing connection...
        {:else}
          {displayTestResult.message}
        {/if}
      </div>
    {/if}

    <div class="modal-actions">
      <button class="btn" onclick={handleTest}>Test</button>
      <div class="spacer"></div>
      <button class="btn" onclick={onCancel}>Cancel</button>
      <button class="btn btn-primary" onclick={handleSave}>Save</button>
    </div>
  </div>
</div>
