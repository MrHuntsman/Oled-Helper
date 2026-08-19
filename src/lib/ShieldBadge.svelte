<script>
  import { onDestroy } from 'svelte';

  // Windows-style admin shield badge, flat single-color icon (filled bold
  // silhouette + checkmark, one fill, no strokes) signalling a control needs
  // administrator rights. Hovering (or keyboard-focusing) reveals a tooltip.
  //
  //   <ShieldBadge text="Changing RTX HDR requires administrator rights." />
  //
  // Props:
  //   label, accessible name for the icon
  //   text: tooltip content shown on hover/focus
  //   size: icon size in px (inline style so it beats the scoped CSS)
  let { label = 'Requires administrator rights', text = '', size = 18 } = $props();

  let badgeEl = $state(null);
  let tooltipEl = $state(null);
  let tooltipVisible = $state(false);
  let tooltipStyle = $state('');
  let positionFrame = 0;

  function positionTooltip(remeasure = false) {
    cancelAnimationFrame(positionFrame);
    positionFrame = requestAnimationFrame(() => {
      if (!badgeEl || !tooltipEl || !tooltipVisible) return;
      const badge = badgeEl.getBoundingClientRect();
      const tip = tooltipEl.getBoundingClientRect();
      const margin = 8;
      const gap = 8;
      const maxLeft = Math.max(margin, window.innerWidth - tip.width - margin);
      const left = Math.max(margin, Math.min(badge.left, maxLeft));
      const maxWidth = Math.max(0, window.innerWidth - left - margin);
      const belowTop = badge.bottom + gap;
      const aboveTop = badge.top - tip.height - gap;
      const top = belowTop + tip.height <= window.innerHeight - margin
        ? belowTop
        : Math.max(margin, aboveTop);
      tooltipStyle = `left:${Math.round(left)}px;top:${Math.round(top)}px;max-width:${Math.floor(maxWidth)}px;`;
      // Applying the available width can wrap the message and change its
      // height. Measure once more after Svelte applies the style so the
      // below/above decision remains correct at the bottom edge.
      if (!remeasure) positionTooltip(true);
    });
  }

  function showTooltip() {
    if (!text) return;
    tooltipVisible = true;
    positionTooltip();
    window.addEventListener('resize', positionTooltip);
    window.addEventListener('scroll', positionTooltip, true);
  }

  function hideTooltip() {
    tooltipVisible = false;
    cancelAnimationFrame(positionFrame);
    window.removeEventListener('resize', positionTooltip);
    window.removeEventListener('scroll', positionTooltip, true);
  }

  onDestroy(() => {
    hideTooltip();
  });
</script>

<span
  class="admin-badge"
  bind:this={badgeEl}>

  <button
    type="button"
    class="admin-badge-hit"
    tabindex="0"
    aria-label={label}
    onmouseenter={showTooltip}
    onmouseleave={hideTooltip}
    onfocus={showTooltip}
    onblur={hideTooltip}></button>
  <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={label} style="width:{size}px;height:{size}px">
    <path fill-rule="evenodd" clip-rule="evenodd" d="M6.2 4.2L12 2.8L17.8 4.2V9.4C17.8 12.4 15.8 14.9 12 16C8.2 14.9 6.2 12.4 6.2 9.4V4.2Z"/>
    <path d="M9.8 10.1L10.9 11.2L14.3 7.8L15.3 8.8L10.9 13.3L8.8 11.2L9.8 10.1Z"/>
  </svg>
  {#if text}
    <span
      class="admin-tip"
      class:visible={tooltipVisible}
      style={tooltipStyle}
      bind:this={tooltipEl}
      role="tooltip">
      <svg class="admin-tip-icon" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
        <path fill-rule="evenodd" clip-rule="evenodd" d="M6.2 4.2L12 2.8L17.8 4.2V9.4C17.8 12.4 15.8 14.9 12 16C8.2 14.9 6.2 12.4 6.2 9.4V4.2Z"/>
        <path d="M9.8 10.1L10.9 11.2L14.3 7.8L15.3 8.8L10.9 13.3L8.8 11.2L9.8 10.1Z"/>
      </svg>
      {text}
    </span>
  {/if}
</span>

<style>
  .admin-badge {
    position: relative;
    display: inline-flex;
    align-items: center;
    color: #e0a84b;
    margin-left: 0;
  }
  .admin-badge > svg {
    pointer-events: none;
    width: 18px;
    height: 18px;
    display: block;
    filter: drop-shadow(0 0 2px rgba(224, 168, 75, 0.35));
    /* Same optical correction as the tooltip icon: the pointed-bottom
       shield's visual mass sits above its box center, so nudge it down. */
    transform: translateY(2px);
  }
  /* Invisible hit area so hovering anywhere on the badge shows the tip. */
  .admin-badge-hit {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    padding: 0;
    border: none;
    background: transparent;
    cursor: default;
    outline: none;
  }
  /* Viewport-anchored tooltip. Its position is measured in the component so
     it can flip horizontally and clamp to the WebView edges; fixed positioning
     also lets it escape scrollable tab containers that would clip an absolute
     child. */
  .admin-tip {
    position: fixed;
    top: 50%;
    left: 0;
    /* Let short messages use their natural width. The inline positioner
       supplies a viewport-safe maximum only when the text is long. */
    min-width: 0;
    max-width: calc(100vw - 16px);
    display: flex;
    align-items: center;
    gap: 8px;
    background: #1c1c22;
    border: 1px solid var(--divider);
    color: var(--text);
    font-size: 12px;
    font-weight: 400;
    line-height: 1.4;
    padding: 10px 12px;
    border-radius: 6px;
    white-space: normal;
    pointer-events: none;
    z-index: 20;
    box-shadow: 0 4px 10px rgba(0, 0, 0, 0.35);
    opacity: 0;
    transform: translateY(2px) scale(0.96);
    transform-origin: left top;
    transition: opacity 0.12s ease, transform 0.12s ease;
  }
  .admin-tip-icon {
    flex: 0 0 auto;
    width: 14px;
    height: 14px;
    color: #e0a84b;
    /* The shield's visual mass sits above its box center (pointed bottom),
       so nudge it down to align optically with the text. */
    transform: translateY(2px);
  }
  .admin-tip.visible {
    opacity: 1;
    transform: translateY(0) scale(1);
  }
</style>
