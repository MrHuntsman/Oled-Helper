<script>
  // Small subpixel-layout indicator for the rendering mode buttons. Mirrors
  // the tab icon's bars: RGB = red/green/blue, BGR = reversed, Grayscale =
  // three neutral bars. Full-strength, fully opaque colors so the subpixel
  // order is easy to read even at 16-18px.
  let { mode = 'rgb', class: className = '' } = $props();

  const R = '#FF3B30';
  const G = '#34C759';
  const B = '#007AFF';

  const palettes = {
    grayscale: ['#A3A3AD', '#A3A3AD', '#A3A3AD'],
    rgb: [R, G, B],
    bgr: [B, G, R],
  };

  const bars = $derived.by(() => {
    const colors = palettes[mode] ?? palettes.grayscale;
    const three = colors.length === 3;
    return colors.map((color, i) => ({
      color,
      x: three ? 2 + i * 6 : 1 + i * 4,
      width: three ? 4 : 3,
    }));
  });
</script>

<svg class={className} viewBox="0 0 18 14" aria-hidden="true" focusable="false">
  {#each bars as bar}
    <rect x={bar.x} y="2" width={bar.width} height="10" rx="1" fill={bar.color} />
  {/each}
</svg>
