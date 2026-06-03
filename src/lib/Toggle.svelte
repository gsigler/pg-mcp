<script>
  let { label, checked = $bindable(false) } = $props();
</script>

<label class="toggle-label">
  <span>{label}</span>
  <button
    type="button"
    class="toggle-switch"
    class:on={checked}
    role="switch"
    aria-checked={checked}
    aria-label={label}
    onclick={() => (checked = !checked)}
  >
    <span class="toggle-thumb"></span>
  </button>
</label>
