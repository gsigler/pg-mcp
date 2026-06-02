<script>
  let { config, testResults, onActivate, onEdit, onDelete, onTest, onAdd } = $props();
</script>

{#if config.connections.length === 0}
  <div class="empty-state">
    <p>No connections yet.</p>
    <button type="button" class="btn btn-primary" onclick={onAdd}>Add your first connection</button>
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
          <span
            class="test-status"
            class:success={result.success}
            class:error={!result.success && !result.loading}
            class:loading={result.loading}
            title={result.message}
          >
            <span class="status-dot"></span>
            {#if result.loading}
              Testing
            {:else if result.success}
              OK
            {:else}
              Failed
            {/if}
          </span>
        {/if}

        <div class="card-actions" onclick={(e) => e.stopPropagation()}>
          <button
            type="button"
            class="btn btn-small"
            disabled={result?.loading}
            aria-busy={result?.loading}
            onclick={() => onTest(conn.name)}
          >
            {result?.loading ? "Testing" : "Test"}
          </button>
          <button type="button" class="btn btn-small" onclick={() => onEdit(conn.name)}>
            Edit
          </button>
          <button type="button" class="btn btn-small btn-danger" onclick={() => onDelete(conn.name)}>
            Delete
          </button>
        </div>
      </div>
    {/each}
  </div>
{/if}
