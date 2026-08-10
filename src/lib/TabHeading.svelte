<script>
  let { icon, title, subtitle = '' } = $props();
</script>

<div class="tab-heading">
  {#if icon}
    <img class="tab-icon" src={icon} alt="" />
  {/if}
  <h2>{title}</h2>
  {#if subtitle}
    <span class="subtitle">{subtitle}</span>
  {/if}
  <div class="tab-heading-spacer"></div>
</div>
