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
      password = "";
      connectionString = "";
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
      const result = await invoke("test_connection_draft", { connection: buildConnection() });
      localTestResult = { success: true, message: result };
    } catch (e) {
      localTestResult = { success: false, message: String(e) };
    }
  }

  function handleKeydown(e) {
    if (e.key === "Escape") onCancel();
  }

  let displayTestResult = $derived(localTestResult || testResult);

  // When the user pastes a `postgres(ql)://…` URL into the connection-string
  // field, decompose it into the manual fields below and clear the URL
  // field. Two reasons:
  //   1. Removes the ambiguity of "which set is the app actually using?" —
  //      after a paste, only the manual fields hold state.
  //   2. Lets the user see and tweak what was parsed (e.g. fix a port,
  //      flip SSL) without re-editing the URL.
  // Manual parser rather than `new URL` because non-special schemes like
  // `postgresql:` are treated opaquely by the WHATWG URL parser.
  function parseConnectionUrl(input) {
    const cs = (input || "").trim();
    if (!cs) return null;
    const m = cs.match(/^(postgres(?:ql)?):\/\/(.*)$/i);
    if (!m) return null;
    let rest = m[2];

    const fragIdx = rest.indexOf("#");
    if (fragIdx !== -1) rest = rest.slice(0, fragIdx);

    let query = "";
    const qIdx = rest.indexOf("?");
    if (qIdx !== -1) {
      query = rest.slice(qIdx + 1);
      rest = rest.slice(0, qIdx);
    }

    let pathPart = "";
    const slashIdx = rest.indexOf("/");
    if (slashIdx !== -1) {
      pathPart = rest.slice(slashIdx + 1);
      rest = rest.slice(0, slashIdx);
    }

    // Userinfo + hostport. `lastIndexOf('@')` is robust to '@' in passwords
    // when the password contains a literal '@' (which RFC 3986 allows in
    // userinfo as a sub-delim, though it's bad practice).
    let userinfo = "";
    let hostport = rest;
    const atIdx = rest.lastIndexOf("@");
    if (atIdx !== -1) {
      userinfo = rest.slice(0, atIdx);
      hostport = rest.slice(atIdx + 1);
    }

    let userPart = "";
    let passPart = "";
    if (userinfo) {
      const cIdx = userinfo.indexOf(":");
      if (cIdx !== -1) {
        userPart = userinfo.slice(0, cIdx);
        passPart = userinfo.slice(cIdx + 1);
      } else {
        userPart = userinfo;
      }
    }

    let hostPart = hostport;
    let portPart = "";
    if (hostport.startsWith("[")) {
      // IPv6: [::1]:5432
      const closeIdx = hostport.indexOf("]");
      if (closeIdx !== -1) {
        hostPart = hostport.slice(1, closeIdx);
        if (hostport[closeIdx + 1] === ":") {
          portPart = hostport.slice(closeIdx + 2);
        }
      }
    } else {
      const colonIdx = hostport.lastIndexOf(":");
      if (colonIdx !== -1 && /^\d+$/.test(hostport.slice(colonIdx + 1))) {
        hostPart = hostport.slice(0, colonIdx);
        portPart = hostport.slice(colonIdx + 1);
      }
    }

    let params;
    try {
      params = new URLSearchParams(query);
    } catch {
      params = new URLSearchParams();
    }
    const sslmodeRaw = (params.get("sslmode") || "").toLowerCase();
    const wantsSsl =
      sslmodeRaw === "require" ||
      sslmodeRaw === "verify-ca" ||
      sslmodeRaw === "verify-full";

    const safeDecode = (s) => {
      if (!s) return "";
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    };

    return {
      host: safeDecode(hostPart),
      port: portPart ? Number(portPart) : null,
      database: safeDecode(pathPart),
      user: safeDecode(userPart),
      password: safeDecode(passPart),
      ssl: wantsSsl,
    };
  }

  function importConnectionString(event = null) {
    const input = event?.currentTarget;
    const rawValue = input?.value ?? connectionString;
    const parsed = parseConnectionUrl(rawValue);
    if (!parsed) return;
    if (parsed.host) host = parsed.host;
    if (parsed.port) port = parsed.port;
    if (parsed.database) database = parsed.database;
    if (parsed.user) user = parsed.user;
    if (parsed.password) password = parsed.password;
    if (parsed.ssl) ssl = true;
    connectionString = "";
    if (input) input.value = "";
  }
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
      <label for="conn-string">Paste Connection String</label>
      <input
        id="conn-string"
        type="password"
        bind:value={connectionString}
        onchange={(e) => importConnectionString(e)}
        onblur={(e) => importConnectionString(e)}
        onpaste={() => setTimeout(importConnectionString, 0)}
        placeholder="postgresql://user:pass@host:5432/db"
      />
      <div class="helper-text">Auto-fills the fields below.</div>
    </div>

    <div class="divider">
      <span>connection details</span>
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
