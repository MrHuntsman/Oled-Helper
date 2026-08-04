<script>
  // Inlined data URI — see App.svelte for why icons never hit the network.
  import tabAboutIcon from './icons/tab_about.png?inline';
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src={tabAboutIcon} alt="" />
    <h2>About</h2>
  </div>
  <div class="separator"></div>
  <p style="color: var(--text-weak);">OLED Helper — about text, version, update check go here.</p>
</div>
