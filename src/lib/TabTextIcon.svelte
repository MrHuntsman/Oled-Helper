<script>
  // Inline SVG icon for the Text rendering tab: a subpixel RGB layout (three
  // vertical bars, red-green-blue) with no letters. Crisp at any size, and
  // colors are fixed to the classic subpixel palette.
  let { class: className = '' } = $props();
</script>

<svg class={className} viewBox="0 0 64 64" aria-hidden="true" focusable="false">
  <rect x="8" y="8" width="12" height="48" rx="3" fill="#E84C4C" />
  <rect x="26" y="8" width="12" height="48" rx="3" fill="#56C476" />
  <rect x="44" y="8" width="12" height="48" rx="3" fill="#588AF0" />
</svg>
