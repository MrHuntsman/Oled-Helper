<script>
  // Inlined data URI — see App.svelte for why icons never hit the network.
  import tabSystemIcon from './icons/tab_system.png?inline';
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src={tabSystemIcon} alt="" />
    <h2>System</h2>
  </div>
  <div class="separator"></div>
  <p style="color: var(--text-weak);">Startup toggle, misc system options go here.</p>
</div>
