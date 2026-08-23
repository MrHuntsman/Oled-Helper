<script>
  // Inline SVG icon for the Windows Font tab: the classic "Aa" font-selector
  // glyph, drawn with currentColor so it follows the nav/heading text color.
  // Rendered in Inter, the app's own UI font and the first family in the
  // tab's Google Fonts dropdown, so the icon matches the app's font
  // ecosystem. Bold keeps it readable at nav size.
  let { class: className = '' } = $props();
</script>

<svg class={className} viewBox="0 0 64 64" aria-hidden="true" focusable="false">
  <text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle"
    font-family="'Inter', 'Segoe UI', sans-serif" font-size="48" font-weight="700"
    fill="currentColor">Aa</text>
</svg>
