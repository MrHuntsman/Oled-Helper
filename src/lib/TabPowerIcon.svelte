<script>
  // Inline SVG icon for the Power tab: the universal power symbol (a broken
  // circle with a vertical bar), drawn with currentColor so it follows the
  // nav/heading text color. Crisp at any size.
  let { class: className = '' } = $props();
</script>

<svg class={className} viewBox="0 0 64 64" aria-hidden="true" focusable="false">
  <path d="M32 6v24" stroke="currentColor" stroke-width="7" stroke-linecap="round" />
  <path d="M17 17a21 21 0 1 0 30 0" fill="none" stroke="currentColor" stroke-width="7" stroke-linecap="round" />
</svg>
