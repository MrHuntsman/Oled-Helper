<script>
  // Small subpixel-layout indicator for the rendering mode buttons. Mirrors
  // the tab icon's three-bar glyph: RGB = red/green/blue, BGR = reversed,
  // Grayscale = three neutral bars. Full-strength, fully opaque colors so the
  // subpixel order is easy to read even at 16-18px.
  let { mode = 'rgb', class: className = '' } = $props();

  const grays = ['#A3A3AD', '#A3A3AD', '#A3A3AD'];
  const rgb = ['#FF3B30', '#34C759', '#007AFF'];
  const palette = $derived(mode === 'rgb' ? rgb : mode === 'bgr' ? [...rgb].reverse() : grays);
</script>

<svg class={className} viewBox="0 0 18 14" aria-hidden="true" focusable="false">
  {#each palette as color, i}
    <rect x={2 + i * 6} y="2" width="4" height="10" rx="1" fill={color} />
  {/each}
</svg>
