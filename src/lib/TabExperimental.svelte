<script>
  import { invoke } from '@tauri-apps/api/core';
  import { dwmEotf, setDwmEotfProfile, setDwmEotfAutoReapply } from './stores.svelte.js';
  import tabDebugIcon from './icons/tab_debug.png?inline';

  let scanning = $state(false);
  let scan = $state(null);
  let error = $state('');
  let copied = $state(false);
  let dumping = $state(false);
  let digestDump = $state(null);
  let digestCopied = $state(false);

  function formatScanResults() {
    if (!scan) return '';
    const rows = [
      `OLED Helper \u2014 DWM EOTF scan results`,
      `=====================================`,
      ``,
      `Status:        ${scan.status}`,
      `Message:       ${scan.message}`,
      `DWM PID:       ${scan.dwm_pid || '\u2014'}`,
      `dwmcore.dll:   ${scan.dwmcore_size ? `${Math.round(scan.dwmcore_size / 1024 / 1024)} MB` : '\u2014'}`,
      `Exec regions:  ${scan.readable_regions}`,
      `Scanned:       ${Math.round((scan.scanned_bytes || 0) / 1024 / 1024)} MB`,
      `Total hits:    ${scan.constant_hits}`,
      `Constants:     ${(scan.constants_found || []).join(', ') || 'none'}`,
      `DXBC shaders:  ${scan.dxbc_containers || 0}`,
      `Whitelist:     ${scan.whitelist_hits ?? 0} / 4`,
      `Gamma profile: ${gammaProfile}${GAMMA_PROFILES.find(p => p.id === gammaProfile)?.label ? ' (' + GAMMA_PROFILES.find(p => p.id === gammaProfile).label + ')' : ''}`,
      `Debug priv:    ${scan.debug_privilege_used ? 'yes' : 'no'}`,
    ];
    if (scan.whitelist_matched?.length) {
      rows.push('');
      rows.push('Matched digests:');
      for (const digest of scan.whitelist_matched) rows.push(`  ${digest}`);
    }
    return rows.join('\n');
  }

  async function copyResults() {
    if (!scan) return;
    try {
      await navigator.clipboard.writeText(formatScanResults());
      copied = true;
      setTimeout(() => copied = false, 1800);
    } catch {
      // clipboard unavailable
    }
  }

  async function runScan() {
    if (scanning) return;
    scanning = true;
    error = '';
    copied = false;
    try {
      scan = await invoke('scan_dwm_eotf');
    } catch (cause) {
      error = String(cause);
      scan = null;
    } finally {
      scanning = false;
    }
  }

  async function runScanWithDebugPrivilege() {
    if (scanning) return;
    scanning = true;
    error = '';
    copied = false;
    try {
      scan = await invoke('scan_dwm_eotf_debug_priv');
    } catch (cause) {
      error = String(cause);
      scan = null;
    } finally {
      scanning = false;
    }
  }

  async function runShaderWhitelistScan() {
    if (scanning) return;
    scanning = true;
    error = '';
    copied = false;
    try {
      scan = await invoke('scan_dwm_shaders');
    } catch (cause) {
      error = String(cause);
      scan = null;
    } finally {
      scanning = false;
    }
  }

  let patching = $state(false);
  let gammaProfile = $state('2.2');

  const GAMMA_PROFILES = [
    { id: 'srgb', label: 'sRGB piecewise (restore)' },
    { id: '2.2', label: 'Gamma 2.2' },
    { id: '2.4', label: 'Gamma 2.4' },
    { id: '2.6', label: 'Gamma 2.6' },
  ];

  async function runApplyPatch() {
    if (patching || scanning) return;
    patching = true;
    error = '';
    copied = false;
    try {
      scan = await invoke('apply_dwm_patch', { profile: gammaProfile });
      // Persist the applied profile so the backend watcher re-applies it after
      // DWM restarts. Restoring sRGB clears the override (fresh DWM is already
      // sRGB, so re-applying it would just restart DWM pointlessly).
      if (scan && scan.status === 'patched') {
        setDwmEotfProfile(gammaProfile === 'srgb' ? '' : gammaProfile);
      }
    } catch (cause) {
      error = String(cause);
      scan = null;
    } finally {
      patching = false;
    }
  }

  async function runDigestDump() {
    if (dumping || scanning || patching) return;
    dumping = true;
    digestCopied = false;
    error = '';
    try {
      digestDump = await invoke('dump_dwm_shader_digests');
    } catch (cause) {
      error = String(cause);
      digestDump = null;
    } finally {
      dumping = false;
    }
  }

  function formatDigestDump() {
    if (!digestDump) return '';
    const rows = [
      `OLED Helper \u2014 DWM shader digest dump`,
      `========================================`,
      `Status:        ${digestDump.status}`,
      `Message:       ${digestDump.message}`,
      `DWM PID:       ${digestDump.dwm_pid || '\u2014'}`,
      `Unique digests: ${digestDump.total}`,
      `Whitelisted:   ${digestDump.whitelisted_hits}`,
    ];
    if (digestDump.digests?.length) {
      rows.push('');
      rows.push('Digests (whitelisted first):');
      for (const entry of digestDump.digests) {
        rows.push(`  ${entry.digest}  ${entry.whitelisted ? 'whitelisted' : ''}  file=${entry.file_size}`);
      }
    }
    return rows.join('\n');
  }

  async function copyDigestDump() {
    if (!digestDump) return;
    try {
      await navigator.clipboard.writeText(formatDigestDump());
      digestCopied = true;
      setTimeout(() => digestCopied = false, 1800);
    } catch {
      // clipboard unavailable
    }
  }

  async function runDryRun() {
    if (scanning) return;
    scanning = true;
    error = '';
    copied = false;
    try {
      scan = await invoke('scan_dwm_dry_run', { profile: gammaProfile });
    } catch (cause) {
      error = String(cause);
      scan = null;
    } finally {
      scanning = false;
    }
  }

  async function openAsAdmin() {
    try {
      await invoke('relaunch_elevated', { tab: 'experimental' });
    } catch (cause) {
      error = String(cause);
    }
  }
</script>

<div class="tab-body experimental-tab">
  <div class="tab-heading">
    <h2>Experimental</h2>
    <span class="subtitle">— DWM EOTF research</span>
    <div class="tab-heading-spacer"></div>
  </div>
  <div class="separator"></div>

  <section class="experimental-intro" aria-labelledby="experimental-safety-title">
    <div class="experimental-intro-icon" aria-hidden="true">
      <img src={tabDebugIcon} alt="" />
    </div>
    <div>
      <h3 id="experimental-safety-title">Read-only compatibility test</h3>
      <p>This checks whether Windows' Desktop Window Manager exposes the known sRGB transfer-function data used by DWM EOTF research. HDR does not need to be enabled for the read-only compatibility scan.</p>
      <p class="experimental-safety-note"><strong>Safe by design:</strong> it does not suspend DWM, write process memory, restart the desktop compositor, or change display settings.</p>
    </div>
  </section>

  <section class="experimental-card" aria-labelledby="dwm-scan-title">
    <div class="experimental-card-header">
      <div>
        <h3 id="dwm-scan-title">DWM EOTF detector</h3>
        <p>Run this manually after enabling Windows HDR. The scan is bounded and usually completes quickly. If the normal scan is blocked, try the debug-privilege scan — it enables SeDebugPrivilege to access protected DWM modules but still never modifies DWM.</p>
      </div>
      <div class="experimental-scan-buttons">
        <button class="experimental-scan-btn" class:busy={scanning} onclick={runScan} disabled={scanning}>
          {scanning ? 'Scanning…' : scan ? 'Scan again' : 'Run read-only scan'}
        </button>
        <button class="experimental-scan-btn debug-priv" class:busy={scanning} onclick={runScanWithDebugPrivilege} disabled={scanning}>
          {scanning ? 'Scanning…' : 'Scan with SeDebugPrivilege'}
        </button>
        <button class="experimental-scan-btn admin-launch" onclick={runShaderWhitelistScan} disabled={scanning}>
          {scanning ? 'Scanning…' : 'Check shader whitelist'}
        </button>
        <button class="experimental-scan-btn admin-launch" onclick={openAsAdmin}>
          Open as Administrator
        </button>
      </div>
    </div>

    <!-- Gamma EOTF patcher section -->
    <div class="experimental-patch-section" aria-labelledby="dwm-patch-title">
      <div class="experimental-patch-header">
        <div>
          <h3 id="dwm-patch-title">DWM EOTF gamma override</h3>
          <p>Select a target gamma curve and apply it to the four whitelisted DWM shaders. Applying restarts DWM: the screen will flash black and windows will briefly disappear while the compositor restarts, then the patched gamma takes effect. Restart DWM or reboot to restore originals.</p>
        </div>
      </div>
      <div class="experimental-patch-controls">
        <select class="experimental-gamma-select" bind:value={gammaProfile} disabled={patching || scanning}>
          {#each GAMMA_PROFILES as profile}
            <option value={profile.id}>{profile.label}</option>
          {/each}
        </select>
        <button class="experimental-scan-btn debug-priv" class:busy={scanning} onclick={runDryRun} disabled={scanning || patching}>
          {scanning ? 'Scanning…' : 'Dry run'}
        </button>
        <button class="experimental-scan-btn apply-patch" class:busy={patching} onclick={runApplyPatch} disabled={patching || scanning}>
          {patching ? 'Restarting DWM…' : '⚠ Apply'}
        </button>
      </div>
      <label class="experimental-auto-reapply">
        <input type="checkbox" checked={dwmEotf.autoReapply} onchange={(event) => setDwmEotfAutoReapply(event.currentTarget.checked)} disabled={patching || scanning} />
        <span>Auto re-apply after DWM restarts{dwmEotf.profile && dwmEotf.autoReapply ? ` — currently ${GAMMA_PROFILES.find(p => p.id === dwmEotf.profile)?.label || dwmEotf.profile}` : dwmEotf.profile && !dwmEotf.autoReapply ? ' (paused)' : ''}</span>
      </label>
    </div>

    <!-- Whitelist growth: dump every shader digest found in dwmcore -->
    <div class="experimental-digest-section" aria-labelledby="dwm-digest-title">
      <div class="experimental-patch-header">
        <div>
          <h3 id="dwm-digest-title">Capture shader digests</h3>
          <p>On Windows builds where the whitelist finds nothing, dump every DXBC container digest in dwmcore so new shaders can be added to the whitelist. Run this on an unpatched (restored) DWM to capture the original digests.</p>
        </div>
      </div>
      <div class="experimental-patch-controls">
        <button class="experimental-scan-btn" class:busy={dumping} onclick={runDigestDump} disabled={dumping || scanning || patching}>
          {dumping ? 'Dumping…' : digestDump ? 'Dump again' : 'Dump shader digests'}
        </button>
      </div>
      {#if digestDump}
        <div class="experimental-result {digestDump.status === 'ok' ? 'ok' : 'error'}">
          <div class="experimental-result-title-row">
            <div class="experimental-result-title">
              {digestDump.status === 'ok' ? `${digestDump.total} unique shader digests` : digestDump.status}
            </div>
            <button class="experimental-copy-btn" class:success={digestCopied} onclick={copyDigestDump} title={digestCopied ? 'Copied!' : 'Copy digest dump as text'} aria-label="Copy digest dump">
              {#if digestCopied}
                <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
              {:else}
                <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
              {/if}
            </button>
          </div>
          <p>{digestDump.message}</p>
          {#if digestDump.digests?.length}
            <div class="experimental-whitelist">
              <span class="experimental-whitelist-label">Digests{digestDump.whitelisted_hits ? ` · ${digestDump.whitelisted_hits} already whitelisted` : ''}</span>
              {#each digestDump.digests as entry}
                <code class:whitelisted={entry.whitelisted}>{entry.digest}{entry.whitelisted ? ' ✓' : ''}</code>
              {/each}
            </div>
          {/if}
        </div>
      {/if}
    </div>

    {#if error}
      <div class="experimental-result error" role="alert">{error}</div>
    {:else if scan}
      <div class="experimental-result {scan.status}">
        <div class="experimental-result-title-row">
          <div class="experimental-result-title">{scan.status === 'patched' ? `Applied — ${GAMMA_PROFILES.find(p => p.id === gammaProfile)?.label || gammaProfile}` : scan.status === 'patch-skipped' ? 'Patch skipped — no changes' : scan.status === 'dry-run-ready' ? 'Dry run — ready to patch' : scan.status === 'dry-run-skipped' ? 'Dry run — nothing to patch' : scan.status === 'whitelist-match' ? 'Known shaders confirmed' : scan.status === 'constants-found' ? 'Known constants found' : scan.status.replaceAll('-', ' ')}</div>
          <button class="experimental-copy-btn" class:success={copied} onclick={copyResults} title={copied ? 'Copied!' : 'Copy scan results as text'} aria-label="Copy scan results">
            {#if copied}
              <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
            {:else}
              <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
            {/if}
          </button>
        </div>
        <p>{scan.message}</p>
        <div class="experimental-metrics">
          <div><span>DWM process</span><strong>{scan.dwm_pid || '—'}</strong></div>
          <div><span>dwmcore.dll</span><strong>{scan.dwmcore_size ? `${Math.round(scan.dwmcore_size / 1024 / 1024)} MB` : '—'}</strong></div>
          <div><span>Readable regions</span><strong>{scan.readable_regions}</strong></div>
          <div><span>Scanned</span><strong>{Math.round((scan.scanned_bytes || 0) / 1024 / 1024)} MB</strong></div>
          <div><span>Constants · vec3</span><strong>{scan.status === 'constants-found' ? '4 + 4' : `${scan.constant_hits > 4 ? '4' : Math.min(scan.constant_hits, 4)} · ${Math.max(0, scan.constant_hits - 4)}`}</strong></div>
          <div><span>DXBC containers</span><strong>{scan.dxbc_containers || '—'}</strong></div>
          <div><span>Whitelist hits</span><strong>{scan.whitelist_hits ?? '—'} / 4</strong></div>
        </div>
        {#if scan.whitelist_matched?.length > 0}
          <div class="experimental-whitelist">
            <span class="experimental-whitelist-label">Matched shader digests</span>
            {#each scan.whitelist_matched as digest}
              <code>{digest}</code>
            {/each}
          </div>
        {/if}
        <div class="experimental-constants">
          {#each ['2.4', '0.04045', '0.055', '0.94786733'] as constant}
            <span class:found={scan.constants_found?.includes(constant)} class:f32={true}>{constant}</span>
          {/each}
          {#each ['2.4 (vec3)', '0.04045 (vec3)', '0.055 (vec3)', '0.94786733 (vec3)'] as constant}
            <span class:found={scan.constants_found?.includes(constant)} class:vec3={true}>{constant.replace(' (vec3)', '')}</span>
          {/each}
        </div>
      </div>
    {:else}
      <div class="experimental-idle">No scan has been run. Nothing is changed until you press the button.</div>
    {/if}
  </section>

  <section class="experimental-warning">
    <strong>What this result means</strong>
    <p>Finding the constants is only a compatibility signal. It does not identify a patchable shader, grant permission to modify DWM, or guarantee that Chromium/WebView2 uses the same conversion path.</p>
  </section>
</div>
