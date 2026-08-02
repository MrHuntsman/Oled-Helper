<script>
  import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart';
  import { onMount } from 'svelte';

  let startOnBoot = $state(false);
  let ready = $state(false);

  onMount(async () => {
    try {
      startOnBoot = await isEnabled();
    } catch (e) {
      console.warn('[settings] isEnabled failed', e);
    } finally {
      ready = true;
    }
  });

  async function toggleStartOnBoot() {
    const next = !startOnBoot;
    startOnBoot = next; // optimistic — reverted below on failure
    try {
      if (next) await enable();
      else await disable();
    } catch (e) {
      console.warn('[settings] toggle autostart failed', e);
      startOnBoot = !next;
    }
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src="/icons/tab_settings.png" alt="" />
    <h2>Settings</h2>
  </div>
  <div class="separator"></div>

  <button class="settings-row" disabled={!ready} onclick={toggleStartOnBoot}>
    <div class="settings-row-text">
      <span class="settings-row-label">Start on Boot</span>
      <span class="settings-row-desc">Launch OLED Helper automatically when Windows starts.</span>
    </div>
    <div class="pill-wrap">
      <span class={`pill${startOnBoot ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
  </button>
</div>
