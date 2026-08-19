<script>
  // Vendor-exclusivity tag, a small pill shown next to settings that only
  // work on one GPU vendor (e.g. "RTX HDR mode" → NVIDIA).
  //
  //   <VendorTag vendor="nvidia" />
  //
  // Props:
  //   vendor, 'nvidia' | 'amd' | 'intel' (unknown values render nothing)
  export let vendor = '';

  const META = {
    nvidia: { label: 'NVIDIA', color: '#76b900' },
    amd: { label: 'AMD', color: '#ed1c24' },
    intel: { label: 'INTEL', color: '#00c2ff' },
  };
  const meta = META[vendor] ?? null;
</script>

{#if meta}
  <span class="vendor-tag" style="--vendor:{meta.color}">{meta.label}</span>
{/if}

<style>
  .vendor-tag {
    display: inline-flex;
    align-items: center;
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.05em;
    line-height: 1;
    padding: 2px 6px;
    border-radius: 4px;
    text-transform: uppercase;
    color: var(--vendor);
    background: color-mix(in srgb, var(--vendor) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--vendor) 35%, transparent);
  }
</style>
