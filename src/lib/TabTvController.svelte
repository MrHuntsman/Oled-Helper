<script>
  import { onMount, untrack } from 'svelte';
  import TabHeading from './TabHeading.svelte';
  import tabScreenDimmerIcon from './icons/tab_screen_dimmer.png?inline';
  import {
    tvController,
    pairTv,
    connectTv,
    disconnectTv,
    forgetTv,
    refreshTvPictureContext,
    restartTv,
    setTvPictureMode,
    setTvPictureSetting,
    runtimeLogSettings,
    tvDiscovery,
    discoverTvs,
  } from './stores.svelte.js';

  let host = $state(tvController.host || '');
  let name = $state(tvController.name || '');
  let error = $state('');
  let selectedPictureMode = $state('');
  let status = $state({
    inputId: tvController.inputId || '',
    inputLabel: tvController.inputLabel || '',
    pictureMode: tvController.pictureMode || null,
    pictureModes: [],
    pictureSettings: [],
    pictureSettingsError: '',
    volume: tvController.volume,
    muted: tvController.muted,
    pictureError: tvController.pictureError || '',
    pictureWriteError: tvController.pictureWriteError || '',
  });
  // Per setting pending and error flags are keyed by setting id so one slow
  // alert write never blocks the other controls.
  let settingPending = $state({});
  let settingErrors = $state({});
  let settingDrafts = $state({});
  let inFlightWrites = {};
  let debounceTimers = {};
  const STATUS_REFRESH_INTERVAL = 5000;
  let refreshInFlight = $state(false);
  let manualRefreshInFlight = $state(false);
  let refreshQueued = false;
  let refreshQueuedKind = '';
  let refreshDisposed = false;
  let statusRevision = 0;
  let statusEffectReady = false;

  function safeLogText(value) {
    const text = String(value);
    const lower = text.toLowerCase();
    if (
      lower.includes('client-key')
      || lower.includes('client key')
      || lower.includes('credential')
      || lower.includes('pairing key')
    ) return 'protected detail redacted';
    const hasSecretLikeToken = text.split(/\s+/).some((token) => {
      const clean = token.replace(/^[^A-Za-z0-9_=+/.-]+|[^A-Za-z0-9_=+/.-]+$/g, '');
      return clean.length >= 16 && /^[A-Za-z0-9_=+/.-]+$/.test(clean);
    });
    if (hasSecretLikeToken) return 'protected detail redacted';
    return text.replace(/[^\x20-\x7E]/g, ' ').slice(0, 160);
  }

  function normalizePictureModes(value) {
    if (!Array.isArray(value)) return [];
    return value
      .filter((option) => option && typeof option === 'object')
      .map((option) => ({
        id: typeof option.id === 'string' ? option.id : '',
        label: typeof option.label === 'string' ? option.label : '',
      }))
      .filter((option) => option.id && option.label);
  }

  function samePictureModes(left, right) {
    return left.length === right.length
      && left.every((option, index) => (
        option.id === right[index]?.id && option.label === right[index]?.label
      ));
  }

  function normalizePictureSettings(value) {
    if (!Array.isArray(value)) return [];
    return value
      .filter((setting) => setting && typeof setting === 'object')
      .map((setting) => ({
        id: typeof setting.id === 'string' ? setting.id : '',
        label: typeof setting.label === 'string' ? setting.label : '',
        kind: typeof setting.kind === 'string' ? setting.kind : 'number',
        min: Number.isFinite(Number(setting.min)) ? Number(setting.min) : 0,
        max: Number.isFinite(Number(setting.max)) ? Number(setting.max) : 100,
        value: setting.value == null ? null : String(setting.value),
        supported: setting.supported !== false,
      }))
      .filter((setting) => setting.id && setting.label);
  }

  function samePictureSettings(left, right) {
    return left.length === right.length
      && left.every((setting, index) => (
        setting.id === right[index]?.id
          && setting.label === right[index]?.label
          && setting.kind === right[index]?.kind
          && setting.value === right[index]?.value
          && setting.supported === right[index]?.supported
      ));
  }

  function isHdrPictureMode(mode) {
    return typeof mode === 'string'
      && (mode.startsWith('hdr') || mode.startsWith('dolbyHdr'));
  }

  function applyStatusSource(source) {
    if (!source || typeof source !== 'object') return;
    status.inputId = typeof source.inputId === 'string' ? source.inputId : '';
    status.inputLabel = typeof source.inputLabel === 'string' ? source.inputLabel : '';
    status.pictureMode = typeof source.pictureMode === 'string' ? source.pictureMode : null;
    status.volume = source.volume == null || !Number.isFinite(Number(source.volume))
      ? null
      : Number(source.volume);
    status.muted = typeof source.muted === 'boolean' ? source.muted : null;
    status.pictureError = typeof source.pictureError === 'string' ? source.pictureError : '';
    status.pictureWriteError = typeof source.pictureWriteError === 'string'
      ? source.pictureWriteError
      : '';
    status.pictureSettingsError = typeof source.pictureSettingsError === 'string'
      ? source.pictureSettingsError
      : '';
    const nextPictureModes = normalizePictureModes(source.pictureModes);
    if (!samePictureModes(status.pictureModes, nextPictureModes)) {
      status.pictureModes = nextPictureModes;
    }
    const nextPictureSettings = normalizePictureSettings(source.pictureSettings);
    if (!samePictureSettings(status.pictureSettings, nextPictureSettings)) {
      // Track whether the TV itself changed any reported value, for example
      // after a picture mode switch, a remote control change, or a firmware
      // clamp. Those changes must reach the controls even though the list
      // shape stayed the same.
      let tvValueChanged = false;
      const previousById = new Map(status.pictureSettings.map((s) => [s.id, s.value]));
      for (const setting of nextPictureSettings) {
        if (!inFlightWrites[setting.id] && previousById.get(setting.id) !== setting.value) {
          settingDrafts[setting.id] = setting.value ?? '';
          tvValueChanged = true;
        }
      }
      status.pictureSettings = nextPictureSettings;
      if (tvValueChanged) {
        for (const id of Object.keys(settingErrors)) {
          if (settingErrors[id] && previousById.get(id) !== nextPictureSettings.find((s) => s.id === id)?.value) {
            settingErrors[id] = '';
          }
        }
      }
    }
  }

  // Connection events still populate the shared TV state. Status polling is
  // merged into this tab only, so a read cannot wake the app shell or close
  // menus in the status bar.
  $effect(() => {
    const source = {
      connectionState: tvController.state,
      pending: tvController.pending,
      inputId: tvController.inputId,
      inputLabel: tvController.inputLabel,
      pictureMode: tvController.pictureMode,
      pictureModes: tvController.pictureModes,
      pictureSettings: tvController.pictureSettings,
      pictureSettingsError: tvController.pictureSettingsError,
      volume: tvController.volume,
      muted: tvController.muted,
      pictureError: tvController.pictureError,
      pictureWriteError: tvController.pictureWriteError,
    };
    untrack(() => applyStatusSource(source));
    if (statusEffectReady) statusRevision += 1;
    else statusEffectReady = true;
  });

  const hasSavedDevice = $derived(Boolean(tvController.deviceId));
  // A paired TV keeps its device id. While nothing is paired the tab shows
  // only the pairing section; the video controls need a connection anyway.
  const hasPairedTv = $derived(hasSavedDevice || tvController.state === 'connected');
  const savedTarget = $derived(
    hasSavedDevice && host.trim() === tvController.host && tvController.credentialSaved,
  );
  const shouldReconnect = $derived(
    savedTarget && tvController.state !== 'pairing_required',
  );
  const stateLabel = $derived(
    tvController.state === 'awaiting_approval'
      ? 'Waiting for TV approval'
      : tvController.state === 'connected'
        ? 'Connected'
        : tvController.state === 'connecting'
          ? 'Connecting'
          : tvController.state === 'reconnecting'
            ? 'Reconnecting'
            : tvController.state === 'pairing_required'
              ? 'Pairing required'
              : tvController.state === 'authentication_rejected'
                ? 'Pairing rejected'
                : tvController.state === 'offline'
                  ? 'Offline'
                  : 'Disconnected',
  );
  const connectLabel = $derived(
    tvController.pending
      ? shouldReconnect
        ? 'Connecting…'
        : 'Pairing…'
      : shouldReconnect
        ? 'Connect'
        : hasSavedDevice
          ? 'Pair again'
          : 'Pair TV',
  );
  const discoveryPending = $derived(tvDiscovery.state === 'scanning');
  const currentInput = $derived(
    status.inputLabel || status.inputId || 'Unknown input',
  );
  const currentPictureMode = $derived(status.pictureMode || 'Unavailable');
  const canApplyPictureMode = $derived(
    tvController.state === 'connected'
      && !tvController.pending
      && !refreshInFlight
      && Boolean(status.pictureMode)
      && Boolean(selectedPictureMode)
      && selectedPictureMode !== status.pictureMode,
  );
  const visiblePictureSettings = $derived(
    status.pictureSettings.filter((setting) => (
      setting.supported
      && (setting.id !== 'hdrDynamicToneMapping' || isHdrPictureMode(status.pictureMode))
    )),
  );
  const hasPictureSettings = $derived(visiblePictureSettings.length > 0);
  const pictureSettingsBlocked = $derived(
    tvController.state !== 'connected' || tvController.pending,
  );
  const DTM_SETTING_OPTIONS = [
    { value: 'off', label: 'Off' },
    { value: 'on', label: 'On' },
    { value: 'HGIG', label: 'HGIG' },
  ];
  const COLOR_TEMPERATURE_PRESET_OPTIONS = [
    { value: 'warm2', label: 'Warm 2' },
    { value: 'warm1', label: 'Warm 1' },
    { value: 'normal', label: 'Normal' },
    { value: 'cool1', label: 'Cool 1' },
    { value: 'cool2', label: 'Cool 2' },
  ];

  function settingDraftValue(setting) {
    const draft = settingDrafts[setting.id];
    return draft === undefined || draft === '' ? (setting.value ?? '') : draft;
  }

  function clampNumericDraft(setting, delta) {
    const current = Number(settingDrafts[setting.id] ?? setting.value ?? 0);
    const base = Number.isFinite(current) ? current : 0;
    const next = Math.min(setting.max, Math.max(setting.min, base + delta));
    settingDrafts[setting.id] = String(Math.round(next));
    settingErrors[setting.id] = '';
    scheduleSettingWrite(setting);
  }

  function onNumericDraftInput(setting) {
    settingErrors[setting.id] = '';
    // Only arm the debounced write once the typed draft is a complete
    // in range number, so intermediate states like an empty field or a
    // lone minus sign never fire a request.
    const parsed = Number(settingDrafts[setting.id]);
    if (settingDrafts[setting.id] === '' || !Number.isInteger(parsed)) return;
    if (parsed < setting.min || parsed > setting.max) return;
    scheduleSettingWrite(setting);
  }

  function onEnumDraftChange(setting) {
    settingErrors[setting.id] = '';
    scheduleSettingWrite(setting, 0);
  }

  // Stepper clicks are debounced so a burst of clicks becomes one TV write,
  // the same way the TV remote behaves. Dropdowns apply immediately.
  function scheduleSettingWrite(setting, delay = 600) {
    if (delay > 0) {
      clearTimeout(debounceTimers[setting.id]);
      debounceTimers[setting.id] = setTimeout(() => {
        delete debounceTimers[setting.id];
        void writeSetting(setting.id);
      }, delay);
    } else {
      clearTimeout(debounceTimers[setting.id]);
      delete debounceTimers[setting.id];
      void writeSetting(setting.id);
    }
  }

  async function writeSetting(settingId) {
    if (pictureSettingsBlocked || inFlightWrites[settingId]) return;
    const setting = status.pictureSettings.find((entry) => entry.id === settingId);
    if (!setting) return;
    const value = settingDrafts[settingId];
    if (value === undefined || value === '' || String(value) === String(setting.value ?? '')) {
      return;
    }
    error = '';
    settingErrors[settingId] = '';
    settingPending[settingId] = true;
    inFlightWrites[settingId] = true;
    const startedAt = performance.now();
    console.debug('[tv] picture setting write requested', { settingId, value });
    try {
      const snapshot = await setTvPictureSetting(settingId, String(value));
      console.debug('[tv] picture setting write request completed', {
        settingId,
        value,
        elapsedMs: Math.round(performance.now() - startedAt),
      });
      if (snapshot && typeof snapshot === 'object') {
        // The reply carries the post write readback. Clear the in flight
        // guard first so the applied values reach the drafts.
        delete inFlightWrites[settingId];
        applyStatusSource(snapshot);
      }
    } catch (e) {
      console.warn('[tv] picture setting write request failed', {
        settingId,
        value,
        elapsedMs: Math.round(performance.now() - startedAt),
        error: safeLogText(e),
      });
      settingErrors[settingId] = safeLogText(e);
      delete inFlightWrites[settingId];
      // Snap the draft back to what the TV last reported so the control
      // does not keep showing a value the TV refused.
      const current = status.pictureSettings.find((entry) => entry.id === settingId);
      if (current) settingDrafts[settingId] = current.value ?? '';
    } finally {
      settingPending[settingId] = false;
      delete inFlightWrites[settingId];
    }
  }

  $effect(() => {
    const current = status.pictureMode || '';
    const selectedStillAvailable = status.pictureModes.some(
      (option) => option.id === selectedPictureMode,
    );
    if (selectedStillAvailable) return;
    selectedPictureMode = status.pictureModes.some((option) => option.id === current)
      ? current
      : (status.pictureModes[0]?.id ?? '');
  });

  async function findTvs() {
    error = '';
    console.debug('[tv] discovery requested');
    try {
      await discoverTvs();
      console.debug('[tv] discovery request completed', { candidateCount: tvDiscovery.candidates.length });
    } catch (e) {
      console.warn('[tv] discovery request failed', { error: safeLogText(e) });
      error = String(e);
    }
  }

  function selectCandidate(candidate) {
    host = candidate.host;
    if (candidate.name) name = candidate.name;
    error = '';
  }

  async function connect() {
    error = '';
    const reconnecting = shouldReconnect;
    console.debug('[tv] connection requested', { reconnecting, host: host.trim() });
    try {
      if (reconnecting) {
        await connectTv(host, name || 'OLED Helper');
      } else {
        await pairTv(host, name || 'OLED Helper');
      }
      console.debug('[tv] connection request completed', { reconnecting });
    } catch (e) {
      console.warn('[tv] connection request failed', { reconnecting, error: safeLogText(e) });
      error = String(e);
    }
  }

  async function disconnect() {
    error = '';
    console.debug('[tv] disconnect requested');
    try {
      await disconnectTv();
      console.debug('[tv] disconnect request completed');
    } catch (e) {
      console.warn('[tv] disconnect request failed', { error: safeLogText(e) });
      error = String(e);
    }
  }

  async function forget() {
    error = '';
    if (!globalThis.confirm('Forget this TV pairing? OLED Helper will remove the saved credential from Windows Credential Manager.')) {
      return;
    }
    console.debug('[tv] forget pairing requested');
    try {
      await forgetTv();
      console.debug('[tv] forget pairing request completed');
    } catch (e) {
      console.warn('[tv] forget pairing request failed', { error: safeLogText(e) });
      error = String(e);
    }
  }

  function refreshDebugLog(kind, message, details = {}) {
    if (runtimeLogSettings.verbosePeriodic || kind === 'manual') {
      console.debug(message, details);
    }
  }

  function canRefreshStatus() {
    return tvController.state === 'connected'
      && !tvController.pending
      && document.visibilityState === 'visible';
  }

  async function refreshPictureContext(kind = 'manual') {
    if (refreshDisposed || !canRefreshStatus()) return;
    if (refreshInFlight) {
      refreshQueued = true;
      if (kind === 'manual') refreshQueuedKind = 'manual';
      refreshDebugLog(kind, '[tv] status refresh coalesced', { requestedKind: kind });
      return;
    }

    refreshInFlight = true;
    if (kind === 'manual') {
      manualRefreshInFlight = true;
      error = '';
    }
    const startedAt = performance.now();
    const startedStatusRevision = statusRevision;
    const beforeInputId = status.inputId;
    const beforePictureMode = status.pictureMode;
    refreshDebugLog(kind, '[tv] status refresh started', {
      kind,
      inputId: beforeInputId,
      pictureMode: beforePictureMode,
    });
    try {
      const snapshot = await refreshTvPictureContext();
      if (!snapshot || typeof snapshot !== 'object') {
        throw new Error('The TV status refresh returned no snapshot');
      }
      if (refreshDisposed) {
        refreshDebugLog(kind, '[tv] status refresh completed after unmount', {
          kind,
          elapsedMs: Math.round(performance.now() - startedAt),
        });
        return;
      }
      if (startedStatusRevision !== statusRevision) {
        refreshDebugLog(kind, '[tv] status refresh result ignored after a TV state event', { kind });
      } else {
        applyStatusSource(snapshot);
      }
      refreshDebugLog(kind, '[tv] status refresh completed', {
        kind,
        elapsedMs: Math.round(performance.now() - startedAt),
        inputId: status.inputId,
        pictureMode: status.pictureMode,
      });
    } catch (e) {
      console.warn('[tv] status refresh failed', {
        kind,
        elapsedMs: Math.round(performance.now() - startedAt),
        error: safeLogText(e),
      });
      if (kind === 'manual') error = String(e);
    } finally {
      refreshInFlight = false;
      if (kind === 'manual') manualRefreshInFlight = false;
      if (!refreshDisposed && refreshQueued) {
        refreshQueued = false;
        const queuedKind = refreshQueuedKind || 'coalesced';
        refreshQueuedKind = '';
        void refreshPictureContext(queuedKind);
      }
    }
  }

  function manualRefresh() {
    void refreshPictureContext('manual');
  }

  let restartInFlight = $state(false);
  let restartStatus = $state('');

  async function restart() {
    error = '';
    if (tvController.state !== 'connected' || tvController.pending || restartInFlight) return;
    if (!globalThis.confirm('Restart this TV? OLED Helper will temporarily disable Quick Start+, power off the TV, send a wake packet, then restore the setting after reconnect.')) {
      return;
    }
    restartInFlight = true;
    restartStatus = 'Restart requested. Waiting for the TV to reconnect.';
    console.debug('[tv] restart requested');
    try {
      await restartTv();
    } catch (e) {
      restartStatus = '';
      error = String(e);
      console.warn('[tv] restart request failed', { error: safeLogText(e) });
    } finally {
      restartInFlight = false;
    }
  }

  let modeWriteInFlight = false;

  async function onPictureModeChange() {
    if (!canApplyPictureMode || modeWriteInFlight) return;
    error = '';
    modeWriteInFlight = true;
    const startedAt = performance.now();
    const requestedMode = selectedPictureMode;
    const inputId = status.inputId;
    console.debug('[tv] picture mode write requested', { requestedMode, inputId });
    try {
      const snapshot = await setTvPictureMode(requestedMode);
      console.debug('[tv] picture mode write request completed', {
        requestedMode,
        inputId,
        elapsedMs: Math.round(performance.now() - startedAt),
      });
      if (snapshot && typeof snapshot === 'object') {
        // The reply carries the post write readback, including the setting
        // values that belong to the newly selected mode.
        applyStatusSource(snapshot);
      }
    } catch (e) {
      console.warn('[tv] picture mode write request failed', {
        requestedMode,
        inputId,
        elapsedMs: Math.round(performance.now() - startedAt),
        error: safeLogText(e),
      });
      error = String(e);
      // Revert the select to the mode the TV actually reports.
      selectedPictureMode = status.pictureMode || '';
    } finally {
      modeWriteInFlight = false;
    }
  }

  onMount(() => {
    refreshDisposed = false;
    if (runtimeLogSettings.verbosePeriodic) {
      console.debug('[tv] controller mounted', {
        visibility: document.visibilityState,
        state: tvController.state,
        activeTab: 'tv-controller',
      });
    }
    void refreshPictureContext('initial');
    const timer = setInterval(() => {
      if (!canRefreshStatus()) return;
      void refreshPictureContext('automatic');
    }, STATUS_REFRESH_INTERVAL);
    return () => {
      refreshDisposed = true;
      refreshQueued = false;
      clearInterval(timer);
      for (const id of Object.keys(debounceTimers)) {
        clearTimeout(debounceTimers[id]);
        delete debounceTimers[id];
      }
      if (runtimeLogSettings.verbosePeriodic) {
        console.debug('[tv] controller unmounted', {
          visibility: document.visibilityState,
          state: tvController.state,
        });
      }
    };
  });
</script>

<div class="tab-body">
  <TabHeading
    icon={tabScreenDimmerIcon}
    title="TV Controller"
    subtitle="Control an LG webOS TV on your local network."
  />
  <div class="separator"></div>

  {#if !hasPairedTv}
    <!-- Pairing only: shown when no TV is paired, replacing the rest of
         the tab so nothing competes for attention before a TV exists. -->
    <div class="dimmer-control tv-pairing-panel">
      <div class="dimmer-control-heading">
        <span class="settings-row-label">Pair an LG webOS TV</span>
        <span
          class="tv-state-chip"
          class:tv-state-pending={tvController.pending}
        >
          {stateLabel}
        </span>
      </div>
      <p class="settings-section-desc">
        Use the TV's numeric IP address. The first connection uses the secure webOS port 3001.
      </p>
      <div class="tv-connect-grid">
        <label class="tv-field">
          <span>TV name</span>
          <input bind:value={name} placeholder="LG C4" disabled={tvController.pending} />
        </label>
        <label class="tv-field">
          <span>TV IP address</span>
          <input
            bind:value={host}
            placeholder="192.168.1.50"
            inputmode="decimal"
            disabled={tvController.pending}
          />
        </label>
      </div>
      <div class="tv-action-row">
        <button
          class="tv-btn"
          onclick={findTvs}
          disabled={discoveryPending || tvController.pending}
        >
          {discoveryPending ? 'Finding…' : 'Find TVs'}
        </button>
        <button
          class="tv-btn tv-btn-primary"
          onclick={connect}
          disabled={tvController.pending || !host.trim()}
        >
          {connectLabel}
        </button>
        {#if hasSavedDevice}
          <button
            class="tv-btn tv-btn-danger"
            onclick={forget}
            disabled={!hasSavedDevice && !tvController.credentialSaved}
          >
            Forget pairing
          </button>
        {/if}
      </div>
      {#if tvController.endpoint}
        <p class="tv-endpoint">{tvController.endpoint}</p>
      {/if}
      {#if hasSavedDevice && !tvController.credentialSaved}
        <p class="settings-section-desc">This pairing is not saved for restart. Pair again to try saving it.</p>
      {/if}
      {#if discoveryPending}
        <p class="tv-pairing-note">Searching the local network for LG webOS TVs.</p>
      {:else if tvDiscovery.error}
        <p class="gpu-error" role="alert">{tvDiscovery.error}</p>
      {:else if tvDiscovery.state === 'complete' && tvDiscovery.candidates.length === 0}
        <p class="tv-pairing-note">No LG webOS TVs answered the search. You can still enter an IP address above.</p>
      {/if}
      {#if tvDiscovery.candidates.length > 0}
        <div class="tv-discovery-list" aria-label="Discovered TVs">
          {#each tvDiscovery.candidates as candidate (candidate.deviceId)}
            <button
              class="tv-discovery-item"
              class:selected={host.trim() === candidate.host}
              aria-pressed={host.trim() === candidate.host}
              onclick={() => selectCandidate(candidate)}
              disabled={tvController.pending}
            >
              <span class="tv-discovery-item-main">
                <span class="tv-discovery-item-name">{candidate.name}</span>
                <span class="tv-discovery-item-model">
                  {[candidate.model, candidate.modelNumber, candidate.resolution]
                    .filter(Boolean)
                    .join(' · ') || 'LG webOS TV'}
                </span>
                <span class="tv-discovery-item-meta">
                  {[candidate.screenType, candidate.softwareVersion]
                    .filter(Boolean)
                    .join(' · ')}
                </span>
              </span>
              <span class="tv-discovery-item-host">{candidate.host}</span>
            </button>
          {/each}
        </div>
      {/if}
      {#if tvController.error || error}
        <p class="gpu-error" role="alert">{error || tvController.error}</p>
      {/if}
      {#if tvController.state === 'awaiting_approval'}
        <p class="tv-pairing-note">Approve the connection request on the TV, then wait for OLED Helper to finish pairing.</p>
      {/if}
      {#if tvController.state === 'pairing_required'}
        <p class="tv-pairing-note">Pair again on the TV to authorize OLED Helper.</p>
      {/if}
    </div>
  {:else}
    <!-- Paired: connection strip plus the video controls. -->
    <div class="dimmer-control tv-pairing-strip">
      <div class="dimmer-control-heading">
        <span
          class="tv-state-chip"
          class:tv-state-connected={tvController.state === 'connected'}
          class:tv-state-pending={tvController.pending}
        >
          {stateLabel}
        </span>
        <span class="tv-pairing-strip-title">
          {tvController.name || 'LG webOS TV'} · {tvController.host}
        </span>
        <button
          class="tv-btn tv-btn-danger tv-pairing-strip-btn"
          onclick={restart}
          disabled={tvController.state !== 'connected' || tvController.pending || restartInFlight}
        >
          {restartInFlight ? 'Restarting…' : 'Restart'}
        </button>
        <button
          class="tv-btn tv-btn-ghost tv-pairing-strip-btn"
          onclick={disconnect}
          disabled={!hasSavedDevice || (tvController.state === 'disconnected' && !tvController.pending)}
        >
          Disconnect
        </button>
        <button
          class="tv-btn tv-btn-ghost tv-pairing-strip-btn"
          onclick={forget}
          disabled={!hasSavedDevice && !tvController.credentialSaved}
        >
          Forget
        </button>
      </div>
      {#if tvController.error || error}
        <p class="gpu-error" role="alert">{error || tvController.error}</p>
      {/if}
      {#if restartStatus}
        <p class="tv-pairing-note">{restartStatus}</p>
      {/if}
      {#if tvController.state === 'reconnecting'}
        <p class="tv-pairing-note">OLED Helper is retrying the saved connection with bounded backoff.</p>
      {/if}
      {#if tvController.state === 'pairing_required'}
        <p class="tv-pairing-note">Pair again on the TV to authorize OLED Helper. Open the pairing view by disconnecting first.</p>
      {/if}
    </div>

    <div class="settings-section-heading">Video</div>
    <div class="dimmer-control tv-video-panel">
      <div class="tv-context-grid">
        <div class="tv-context-item">
          <span class="tv-context-label">Current input</span>
          <span class="tv-context-value">{currentInput}</span>
        </div>
        <div class="tv-context-item">
          <span class="tv-context-label">Picture mode</span>
          <span class="tv-context-value">{currentPictureMode}</span>
        </div>
      </div>
      <div class="tv-picture-mode-row">
        <label class="tv-field" for="tv-picture-mode">
          <span>Picture mode</span>
          <select
            id="tv-picture-mode"
            bind:value={selectedPictureMode}
            onchange={onPictureModeChange}
            disabled={tvController.state !== 'connected' || tvController.pending || status.pictureModes.length === 0}
          >
            {#each status.pictureModes as option (option.id)}
              <option value={option.id}>{option.label}</option>
            {/each}
          </select>
        </label>
        <button
          class="tv-btn tv-pairing-strip-btn"
          onclick={manualRefresh}
          disabled={tvController.state !== 'connected' || tvController.pending || manualRefreshInFlight}
        >
          Refresh
        </button>
        {#if modeWriteInFlight || tvController.pending}
          <span class="tv-setting-pending">Applying…</span>
        {/if}
      </div>
      {#if status.pictureWriteError}
        <p class="gpu-error" role="alert">{status.pictureWriteError}</p>
      {/if}
      {#if status.pictureError}
        <p class="tv-pairing-note">{status.pictureError}</p>
      {/if}
    </div>

    <div class="dimmer-control tv-settings-panel">
      {#if pictureSettingsBlocked || !hasPictureSettings}
        <p class="tv-pairing-note">
          {pictureSettingsBlocked
            ? 'Connect to the TV to read and change its picture settings.'
            : 'The TV did not report its picture setting values yet.'}
        </p>
      {:else}
        <div class="tv-settings-list">
          {#each visiblePictureSettings as setting (setting.id)}
            <div class="tv-setting-row">
              <span class="tv-setting-label">{setting.label}</span>
              {#if setting.kind === 'dtm'}
                <select
                  class="tv-setting-select"
                  bind:value={settingDrafts[setting.id]}
                  onchange={() => onEnumDraftChange(setting)}
                  disabled={pictureSettingsBlocked || settingPending[setting.id]}
                  aria-label={setting.label}
                >
                  {#each DTM_SETTING_OPTIONS as option (option.value)}
                    <option value={option.value}>{option.label}</option>
                  {/each}
                </select>
              {:else if setting.kind === 'colorTemperaturePreset'}
                <select
                  class="tv-setting-select"
                  bind:value={settingDrafts[setting.id]}
                  onchange={() => onEnumDraftChange(setting)}
                  disabled={pictureSettingsBlocked || settingPending[setting.id]}
                  aria-label={setting.label}
                >
                  {#each COLOR_TEMPERATURE_PRESET_OPTIONS as option (option.value)}
                    <option value={option.value}>{option.label}</option>
                  {/each}
                </select>
              {:else}
                <div class="tv-setting-stepper">
                  <button
                    class="tv-btn tv-setting-step-btn"
                    onclick={() => clampNumericDraft(setting, -1)}
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label="Decrease {setting.label}"
                  >−</button>
                  <input
                    class="tv-setting-input"
                    type="number"
                    bind:value={settingDrafts[setting.id]}
                    oninput={() => onNumericDraftInput(setting)}
                    min={setting.min}
                    max={setting.max}
                    step="1"
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label="{setting.label} value"
                  />
                  <button
                    class="tv-btn tv-setting-step-btn"
                    onclick={() => clampNumericDraft(setting, 1)}
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label="Increase {setting.label}"
                  >+</button>
                </div>
              {/if}
              {#if settingPending[setting.id]}
                <span class="tv-setting-pending">Applying…</span>
              {/if}
            </div>
            {#if settingErrors[setting.id]}
              <p class="gpu-error tv-setting-error" role="alert">{settingErrors[setting.id]}</p>
            {/if}
          {/each}
        </div>
        {#if status.pictureSettingsError}
          <p class="tv-pairing-note">{status.pictureSettingsError}</p>
        {/if}
        {#if status.pictureWriteError}
          <p class="gpu-error" role="alert">{status.pictureWriteError}</p>
        {/if}
      {/if}
    </div>
  {/if}
</div>
