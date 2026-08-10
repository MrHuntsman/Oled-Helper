<script>
  // Inlined data URI ,  see App.svelte for why icons never hit the network.
  import tabAboutIcon from './icons/tab_about.png?inline';
  import TabHeading from './TabHeading.svelte';
</script>

<div class="tab-body">
  <TabHeading icon={tabAboutIcon} title="About" />
  <div class="separator"></div>
  <p style="color: var(--text-weak);">OLED Helper is a Windows desktop utility for OLED display calibration and system control.</p>
  <p style="color: var(--text-weak); margin-top: 8px;">v0.1.0 - <a href="https://github.com/" tabindex="0" rel="noopener" style="color: var(--text-weak);">View on GitHub</a></p>
</div>
