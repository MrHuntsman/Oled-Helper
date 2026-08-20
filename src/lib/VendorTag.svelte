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
