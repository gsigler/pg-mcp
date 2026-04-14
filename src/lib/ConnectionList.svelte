<script>
  let { config, testResults, onActivate, onEdit, onDelete, onTest, onAdd } = $props();
</script>

{#if config.connections.length === 0}
  <div class="empty-state">
    <p>No connections yet.</p>
    <button class="btn btn-primary" onclick={onAdd}>+ Add your first database</button>
  </div>
{:else}
  <p class="list-hint">
    Your AI agent can only query the active database. Click a connection to switch.
  </p>
  <div class="connection-list">
    {#each config.connections as conn}
      {@const isActive = conn.name === config.activeConnection}
      {@const result = testResults[conn.name]}
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="connection-card"
        class:active={isActive}
        onclick={() => !isActive && onActivate(conn.name)}
      >
        <span class="color-dot" style="background: {conn.color}"></span>

        <div class="card-main">
          <span class="conn-name">{conn.name}</span>
          <span class="conn-host">{conn.host}:{conn.port}/{conn.database || "—"}</span>
        </div>

        <span class="mode-badge" class:ro={conn.readonly} class:rw={!conn.readonly}>
          {conn.readonly ? "RO" : "RW"}
        </span>

        {#if result}
          <span class="test-dot" class:success={result.success} class:error={!result.success && !result.loading} class:loading={result.loading} title={result.message}></span>
        {/if}

        <div class="card-actions" onclick={(e) => e.stopPropagation()}>
          <button class="icon-btn" onclick={() => onTest(conn.name)} title="Test connection">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M9 11l3 3L22 4"/><path d="M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11"/></svg>
          </button>
          <button class="icon-btn" onclick={() => onEdit(conn.name)} title="Edit">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M11 4H4a2 2 0 00-2 2v14a2 2 0 002 2h14a2 2 0 002-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 013 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>
          </button>
          <button class="icon-btn danger" onclick={() => onDelete(conn.name)} title="Delete">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/></svg>
          </button>
        </div>
      </div>
    {/each}
  </div>
{/if}
