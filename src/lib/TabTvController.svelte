<script>
  import { onMount, untrack } from 'svelte';
  import TabHeading from './TabHeading.svelte';
  import tabScreenDimmerIcon from './icons/tab_screen_dimmer.png?inline';
  import { captureHotkeyCombo } from './hotkey-capture.js';
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
    hotkeys,
    setHotkey,
    tvPictureModeHotkeyAction,
    tvPictureSettingHotkeyAction,
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
  });
  // Per setting pending and error flags are keyed by setting id so one slow
  // alert write never blocks the other controls.
  let pictureSettingsLoading = $state(false);
  let previousConnectionState = tvController.state;
  let settingPending = $state({});
  let settingErrors = $state({});
  let settingDrafts = $state({});
  // One capture at a time across the tab: scope marks the panel that owns the
  // live pill and shows the capture error.
  let hotkeyCapture = $state({ scope: '', action: '', error: '' });
  let inFlightWrites = {};
  // A pick made while that setting's write is still running. It is written
  // next, so switching a value twice quickly keeps the last choice instead
  // of dropping it.
  let queuedSettingWrites = {};
  let settingStepTimers = {};
  const STATUS_REFRESH_INTERVAL = 5000;
  const NUMERIC_SETTING_STEP_DELAY = 500;
  // A picture write is a round trip to the TV plus its readback, and the
  // worker only answers once the write finishes. Cap the wait so a slow or
  // unanswered write cannot pin a control on "Applying…" forever; the next
  // status read reconciles the value the TV really has.
  const TV_WRITE_TIMEOUT = 20000;
  const TV_WRITE_TIMEOUT_MESSAGE = 'The TV has not confirmed this change yet. '
    + 'It may still apply, and the value updates once the TV reports back.';
  // The status read itself rides the same worker as the writes, so it answers
  // only when the worker is idle. Cap the wait so a wedged worker cannot
  // leave the refresh hanging forever.
  const STATUS_REFRESH_TIMEOUT = 10000;
  let refreshInFlight = $state(false);
  let refreshQueued = false;
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
        if (!inFlightWrites[setting.id]
          && !queuedSettingWrites[setting.id]
          && previousById.get(setting.id) !== setting.value) {
          settingDrafts[setting.id] = setting.value ?? '';
          tvValueChanged = true;
        }
      }
      status.pictureSettings = nextPictureSettings;
      if (tvValueChanged) {
        clearSettingErrors();
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
    };
    untrack(() => applyStatusSource(source));
    if (
      statusEffectReady
      && source.connectionState === 'connected'
      && previousConnectionState !== 'connected'
    ) {
      untrack(() => void refreshPictureContext('connected'));
    }
    previousConnectionState = source.connectionState;
    if (source.connectionState !== 'connected') pictureSettingsLoading = false;
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
  const canApplyPictureMode = $derived(
    tvController.state === 'connected'
      && !tvController.pending
      && !refreshInFlight
      && Boolean(status.pictureMode)
      && Boolean(selectedPictureMode)
      && selectedPictureMode !== status.pictureMode,
  );

  function pictureModeHotkeyBinding() {
    const action = tvPictureModeHotkeyAction(selectedPictureMode);
    return action ? (hotkeys[action] || 'None') : 'None';
  }

  function pictureModeHotkeyLabel() {
    return status.pictureModes.find((option) => option.id === selectedPictureMode)?.label || 'picture mode';
  }

  function togglePictureModeHotkeyCapture() {
    const action = tvPictureModeHotkeyAction(selectedPictureMode);
    if (!action) return;
    hotkeyCapture = hotkeyCapture.action === action
      ? { scope: '', action: '', error: '' }
      : { scope: 'pictureMode', action, error: '' };
  }

  function toggleDtmHotkeyCapture() {
    if (!dtmHotkeyAction) return;
    hotkeyCapture = hotkeyCapture.action === dtmHotkeyAction
      ? { scope: '', action: '', error: '' }
      : { scope: 'dtm', action: dtmHotkeyAction, error: '' };
  }

  // The action a pending capture still belongs to, or '' once the field it
  // was started for no longer offers it.
  function liveHotkeyAction() {
    if (hotkeyCapture.scope === 'dtm') return dtmHotkeyAction;
    if (hotkeyCapture.scope === 'pictureMode') return tvPictureModeHotkeyAction(selectedPictureMode);
    return '';
  }

  // Every hotkey field in the tab shares one capture lifecycle; scope only
  // decides which panel reports the result.
  async function applyHotkey(scope, action, combo) {
    if (!action) return;
    const result = await setHotkey(action, combo);
    hotkeyCapture = { scope, action: '', error: result.ok ? '' : result.error };
  }

  function captureHotkey(e) {
    const action = hotkeyCapture.action;
    if (!action) return false;
    const result = captureHotkeyCombo(e);
    if (!result) return false;
    if (result.error) {
      hotkeyCapture = { ...hotkeyCapture, error: result.error };
      return false;
    }
    void applyHotkey(hotkeyCapture.scope, action, result.combo);
    e.preventDefault();
    return true;
  }

  $effect(() => {
    if (!hotkeyCapture.action) return;
    // The field a capture was started for can move underneath it: the picture
    // mode or the Dynamic Tone Mapping value changes, for example from one of
    // the hotkeys being tested. Drop the stale capture instead of leaving the
    // handler below to swallow every key press with no pill showing it.
    if (hotkeyCapture.action !== liveHotkeyAction()) {
      hotkeyCapture = { scope: '', action: '', error: '' };
      return;
    }
    function onKeyDown(e) {
      if (captureHotkey(e)) return;
      e.preventDefault();
      e.stopPropagation();
    }
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  });

  const DTM_SETTING_ID = 'hdrDynamicToneMapping';
  const DTM_SETTING_LABEL = 'Dynamic Tone Mapping';
  const DTM_SETTING_OPTIONS = [
    { value: 'off', label: 'Off' },
    { value: 'on', label: 'On' },
    { value: 'HGIG', label: 'HGIG' },
  ];
  const dtmSetting = $derived(
    status.pictureSettings.find((setting) => setting.id === DTM_SETTING_ID) || null,
  );
  // Only HDR picture modes report and honor the setting, so the panel stays
  // hidden everywhere else, the same way its row in the list used to be.
  const showDtmPanel = $derived(
    Boolean(dtmSetting && dtmSetting.supported && isHdrPictureMode(status.pictureMode)),
  );
  const dtmSelectedValue = $derived(dtmSetting ? settingDraftValue(dtmSetting) : '');
  const dtmHotkeyAction = $derived(
    showDtmPanel ? tvPictureSettingHotkeyAction(DTM_SETTING_ID, dtmSelectedValue) : '',
  );
  const dtmHotkeyBinding = $derived(
    dtmHotkeyAction ? (hotkeys[dtmHotkeyAction] || 'None') : 'None',
  );
  const dtmHotkeyLabel = $derived(
    `${DTM_SETTING_LABEL} ${DTM_SETTING_OPTIONS.find((option) => option.value === dtmSelectedValue)?.label || ''}`.trim(),
  );
  // Dynamic Tone Mapping has its own panel under Picture mode, so the settings
  // list below renders every other setting the TV reported.
  const visiblePictureSettings = $derived(
    status.pictureSettings.filter((setting) => setting.supported && setting.id !== DTM_SETTING_ID),
  );
  const hasPictureSettings = $derived(visiblePictureSettings.length > 0);
  const pictureSettingsBlocked = $derived(
    tvController.state !== 'connected' || tvController.pending,
  );
  const COLOR_GAMUT_SETTING_OPTIONS = [
    { value: 'auto', label: 'Auto' },
    { value: 'native', label: 'Native' },
    { value: 'dynamic', label: 'Dynamic' },
    { value: 'adobeRGB', label: 'Adobe RGB' },
  ];
  const activeSettingError = $derived(
    Object.entries(settingErrors)
      .find(([id, message]) => id !== DTM_SETTING_ID && Boolean(message))?.[1] || '',
  );

  function settingDraftValue(setting) {
    const draft = settingDrafts[setting.id];
    return draft === undefined ? (setting.value ?? '') : draft;
  }

  function sliderDraftValue(setting) {
    const value = settingDraftValue(setting);
    return value === '' ? (setting.value ?? setting.min) : value;
  }

  function settingFillPercent(setting) {
    const value = Number(sliderDraftValue(setting));
    const min = Number(setting.min);
    const max = Number(setting.max);
    const range = max - min;
    if (!Number.isFinite(value) || !Number.isFinite(range) || range <= 0) return 0;
    return Math.min(100, Math.max(0, ((value - min) / range) * 100));
  }

  function clearSettingErrors() {
    for (const id of Object.keys(settingErrors)) {
      settingErrors[id] = '';
    }
  }

  function cancelNumericStep(settingId) {
    const timer = settingStepTimers[settingId];
    if (timer === undefined) return;
    clearTimeout(timer);
    delete settingStepTimers[settingId];
  }

  function clearNumericStepTimers() {
    for (const timer of Object.values(settingStepTimers)) {
      clearTimeout(timer);
    }
    settingStepTimers = {};
  }

  function onNumericSliderInput(setting, value) {
    cancelNumericStep(setting.id);
    settingDrafts[setting.id] = String(value);
    clearSettingErrors();
  }

  function commitNumericSlider(setting) {
    const value = String(settingDrafts[setting.id] ?? '').trim();
    const parsed = Number(value);
    if (value === '' || !Number.isInteger(parsed) || parsed < setting.min || parsed > setting.max) {
      settingErrors[setting.id] = `Enter a whole number between ${setting.min} and ${setting.max}`;
      return;
    }
    void writeSetting(setting.id);
  }

  function stepNumericSetting(setting, delta) {
    if (pictureSettingsBlocked) return;
    if (inFlightWrites[setting.id]) {
      // The draft below is the newer value, so let the running write hand off
      // to it instead of dropping the step.
      queuedSettingWrites[setting.id] = true;
    }
    cancelNumericStep(setting.id);
    const draft = settingDraftValue(setting);
    const parsed = Number(draft);
    const fallback = Number(setting.value ?? setting.min);
    const current = draft !== '' && Number.isFinite(parsed) ? parsed : fallback;
    const min = Number(setting.min);
    const max = Number(setting.max);
    if (!Number.isFinite(current) || !Number.isFinite(min) || !Number.isFinite(max)) return;
    const next = Math.min(max, Math.max(min, current + delta));
    clearSettingErrors();
    settingDrafts[setting.id] = String(next);
    if (next === current) return;
    settingStepTimers[setting.id] = setTimeout(() => {
      delete settingStepTimers[setting.id];
      if (pictureSettingsBlocked) return;
      if (inFlightWrites[setting.id]) {
        queuedSettingWrites[setting.id] = true;
        return;
      }
      void writeSetting(setting.id);
    }, NUMERIC_SETTING_STEP_DELAY);
  }

  function onEnumDraftChange(setting) {
    clearSettingErrors();
    void writeSetting(setting.id);
  }

  async function writeSetting(settingId) {
    if (pictureSettingsBlocked) return;
    if (inFlightWrites[settingId]) {
      // This setting is still being written. Hold the newer pick and apply it
      // when the running write finishes, so switching a value twice quickly
      // ends on the value the user chose last.
      queuedSettingWrites[settingId] = true;
      return;
    }
    cancelNumericStep(settingId);
    const setting = status.pictureSettings.find((entry) => entry.id === settingId);
    if (!setting) return;
    const value = settingDrafts[settingId];
    if (value === undefined || value === '' || String(value) === String(setting.value ?? '')) {
      return;
    }
    error = '';
    clearSettingErrors();
    settingPending[settingId] = true;
    inFlightWrites[settingId] = true;
    const startedAt = performance.now();
    console.debug('[tv] picture setting write requested', { settingId, value });
    try {
      const snapshot = await withTvWriteTimeout(setTvPictureSetting(settingId, String(value)));
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
      // does not keep showing a value the TV refused. A newer pick is left
      // alone, because the queued write below is about to carry it.
      if (!queuedSettingWrites[settingId]) {
        const current = status.pictureSettings.find((entry) => entry.id === settingId);
        if (current) settingDrafts[settingId] = current.value ?? '';
      }
    } finally {
      settingPending[settingId] = false;
      delete inFlightWrites[settingId];
      if (queuedSettingWrites[settingId]) {
        delete queuedSettingWrites[settingId];
        void writeSetting(settingId);
      }
    }
  }

  function withTvWriteTimeout(promise, timeout = TV_WRITE_TIMEOUT, message = TV_WRITE_TIMEOUT_MESSAGE) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(message)), timeout);
      promise.then(
        (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        (error) => {
          clearTimeout(timer);
          reject(error);
        },
      );
    });
  }

  $effect(() => {
    const current = status.pictureMode || '';
    const currentIsAvailable = status.pictureModes.some((option) => option.id === current);
    const selectedStillAvailable = status.pictureModes.some(
      (option) => option.id === selectedPictureMode,
    );
    if (modeWriteInFlight && selectedStillAvailable) return;
    if (currentIsAvailable && selectedPictureMode !== current) {
      selectedPictureMode = current;
      return;
    }
    if (selectedStillAvailable) return;
    selectedPictureMode = currentIsAvailable ? current : (status.pictureModes[0]?.id ?? '');
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
    if (runtimeLogSettings.verbosePeriodic) {
      console.debug(message, details);
    }
  }

  function canRefreshStatus() {
    // A connected TV is polled even while the shared pending flag is set.
    // A write the frontend never issued (a hotkey handled in the tray while
    // the main webview was down) reports its completion only into the
    // backend's cached snapshot, so a rebuilt webview can inherit a pending
    // flag nothing will ever clear. The status read is what proves the
    // worker is idle again and lets the flag be reconciled below.
    return tvController.state === 'connected';
  }

  async function refreshPictureContext(kind = 'automatic') {
    if (refreshDisposed || !canRefreshStatus()) return;
    if (refreshInFlight) {
      refreshQueued = true;
      refreshDebugLog(kind, '[tv] status refresh coalesced', { requestedKind: kind });
      return;
    }

    refreshInFlight = true;
    if (status.pictureSettings.length === 0 && tvController.state === 'connected') {
      pictureSettingsLoading = true;
    }
    const startedAt = performance.now();
    const startedStatusRevision = statusRevision;
    const beforeInputId = status.inputId;
    const beforePictureMode = status.pictureMode;
    // A poll that races a write comes back stale once the write's own reply
    // merges, so only a snapshot taken after the flag flipped can clear it.
    const sharedWasPending = tvController.pending;
    refreshDebugLog(kind, '[tv] status refresh started', {
      kind,
      inputId: beforeInputId,
      pictureMode: beforePictureMode,
    });
    try {
      const snapshot = await withTvWriteTimeout(
        refreshTvPictureContext(),
        STATUS_REFRESH_TIMEOUT,
        'The TV status read timed out',
      );
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
        // The shared pending flag survives a webview rebuild through the
        // backend cache, and tray side writes never emit a clear for it. A
        // status read that the worker answered proves no write is running,
        // so the flag is stale and is cleared here. The revision guard above
        // keeps a poll that raced a real write from clearing it early: the
        // write's own reply applied through a TV state event bumps the
        // revision, and that path clears the flag by itself.
        if (sharedWasPending && tvController.pending) {
          tvController.pending = false;
          console.debug('[tv] stale pending flag cleared after status read');
        }
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
    } finally {
      refreshInFlight = false;
      if (!refreshDisposed && refreshQueued) {
        refreshQueued = false;
        void refreshPictureContext('coalesced');
      } else {
        pictureSettingsLoading = false;
      }
    }
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

  let modeWriteInFlight = $state(false);

  async function onPictureModeChange() {
    if (!canApplyPictureMode || modeWriteInFlight) return;
    clearNumericStepTimers();
    settingDrafts = {};
    clearSettingErrors();
    error = '';
    modeWriteInFlight = true;
    const startedAt = performance.now();
    const requestedMode = selectedPictureMode;
    const inputId = status.inputId;
    console.debug('[tv] picture mode write requested', { requestedMode, inputId });
    try {
      const snapshot = await withTvWriteTimeout(setTvPictureMode(requestedMode));
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
    if (tvController.state === 'connected') {
      void refreshPictureContext('initial');
    }
    const timer = setInterval(() => {
      if (!canRefreshStatus()) return;
      void refreshPictureContext('automatic');
    }, STATUS_REFRESH_INTERVAL);
    // The TV tab is a regular window, not the status bar. Keep polling the
    // TV's input/picture context even while the app is hidden in the tray so
    // the "Current input" / picture mode stays current instead of showing
    // stale values (or "Unknown input") while minimized.
    return () => {
      refreshDisposed = true;
      refreshQueued = false;
      clearInterval(timer);
      clearNumericStepTimers();
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
          class:tv-btn-busy={discoveryPending}
          onclick={findTvs}
          disabled={discoveryPending || tvController.pending}
        >
          <span class="tv-btn-spinner" aria-hidden="true"></span>{discoveryPending ? 'Finding…' : 'Find TVs'}
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
        <p class="tv-pairing-note">
          No LG webOS TVs answered the search. On the TV, turn on All settings → Network → Mobile
          Device Connection, then search again. You can also enter the IP address above.
        </p>
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
        <p class="tv-pairing-note">OLED Helper will keep retrying the saved connection until the TV is available.</p>
      {/if}
      {#if tvController.state === 'pairing_required'}
        <p class="tv-pairing-note">Pair again on the TV to authorize OLED Helper. Open the pairing view by disconnecting first.</p>
      {/if}
    </div>

    <div class="settings-section-heading">Video</div>
    <div class="dimmer-control tv-video-panel">
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
        {#if tvController.state === 'connected'}
          <div class="tv-picture-mode-hotkey">
            <span>Hotkey</span>
            <div class="hotkey-row-controls">
              <button
                type="button"
                class="hotkey-pill"
                class:capturing={hotkeyCapture.action === tvPictureModeHotkeyAction(selectedPictureMode)}
                class:bound={pictureModeHotkeyBinding() !== 'None'}
                onclick={togglePictureModeHotkeyCapture}
                disabled={tvController.pending || modeWriteInFlight || !selectedPictureMode}
                aria-label={`Set hotkey for ${pictureModeHotkeyLabel()}`}
              >
                {hotkeyCapture.action === tvPictureModeHotkeyAction(selectedPictureMode)
                  ? 'Press keys… (Esc to clear)'
                  : pictureModeHotkeyBinding()}
              </button>
              <button
                type="button"
                class="hotkey-clear"
                disabled={pictureModeHotkeyBinding() === 'None'}
                onclick={() => applyHotkey('pictureMode', tvPictureModeHotkeyAction(selectedPictureMode), 'None')}
                title="Clear this hotkey"
                aria-label={`Clear hotkey for ${pictureModeHotkeyLabel()}`}
              >×</button>
            </div>
          </div>
        {/if}
        {#if modeWriteInFlight || tvController.pending}
          <span class="tv-setting-pending">Applying…</span>
        {/if}
      </div>
      {#if hotkeyCapture.scope === 'pictureMode' && hotkeyCapture.error}
        <p class="hotkey-error" role="status">{hotkeyCapture.error}</p>
      {/if}
      {#if status.pictureError}
        <p class="tv-pairing-note">{status.pictureError}</p>
      {/if}
    </div>

    <!-- Dynamic Tone Mapping: same row shape as Picture mode, right below it,
         with a hotkey per value (Off, On, HGIG). -->
    {#if showDtmPanel}
      <div class="dimmer-control tv-dtm-panel">
        <div class="tv-picture-mode-row">
          <label class="tv-field" for="tv-dynamic-tone-mapping">
            <span>{DTM_SETTING_LABEL}</span>
            <select
              id="tv-dynamic-tone-mapping"
              bind:value={settingDrafts[DTM_SETTING_ID]}
              onchange={() => dtmSetting && onEnumDraftChange(dtmSetting)}
              disabled={pictureSettingsBlocked || settingPending[DTM_SETTING_ID]}
              aria-label={DTM_SETTING_LABEL}
            >
              {#each DTM_SETTING_OPTIONS as option (option.value)}
                <option value={option.value}>{option.label}</option>
              {/each}
            </select>
          </label>
          {#if tvController.state === 'connected'}
            <div class="tv-picture-mode-hotkey">
              <span>Hotkey</span>
              <div class="hotkey-row-controls">
                <button
                  type="button"
                  class="hotkey-pill"
                  class:capturing={hotkeyCapture.action === dtmHotkeyAction}
                  class:bound={dtmHotkeyBinding !== 'None'}
                  onclick={toggleDtmHotkeyCapture}
                  disabled={tvController.pending || settingPending[DTM_SETTING_ID] || !dtmHotkeyAction}
                  aria-label={`Set hotkey for ${dtmHotkeyLabel}`}
                >
                  {hotkeyCapture.action === dtmHotkeyAction
                    ? 'Press keys… (Esc to clear)'
                    : dtmHotkeyBinding}
                </button>
                <button
                  type="button"
                  class="hotkey-clear"
                  disabled={dtmHotkeyBinding === 'None'}
                  onclick={() => applyHotkey('dtm', dtmHotkeyAction, 'None')}
                  title="Clear this hotkey"
                  aria-label={`Clear hotkey for ${dtmHotkeyLabel}`}
                >×</button>
              </div>
            </div>
          {/if}
          {#if settingPending[DTM_SETTING_ID]}
            <span class="tv-setting-pending">Applying…</span>
          {/if}
        </div>
        {#if hotkeyCapture.scope === 'dtm' && hotkeyCapture.error}
          <p class="hotkey-error" role="status">{hotkeyCapture.error}</p>
        {/if}
        {#if settingErrors[DTM_SETTING_ID]}
          <p class="gpu-error" role="alert">{settingErrors[DTM_SETTING_ID]}</p>
        {/if}
      </div>
    {/if}

    <div class="dimmer-control tv-settings-panel">
      {#if pictureSettingsBlocked}
        <p class="tv-pairing-note">
          Connect to the TV to read and change its picture settings.
        </p>
      {:else if pictureSettingsLoading}
        <p class="tv-pairing-note" role="status">Reading picture settings…</p>
      {:else if !hasPictureSettings}
        <p class="tv-pairing-note" role="status">
          {status.pictureSettingsError || 'The TV has not reported its picture setting values.'}
        </p>
      {:else}
        <div class="tv-settings-list">
          {#each visiblePictureSettings as setting (setting.id)}
            <div class="tv-setting-row">
              <span class="tv-setting-label">{setting.label}</span>
              {#if setting.kind === 'colorGamut'}
                <select
                  class="tv-setting-select"
                  bind:value={settingDrafts[setting.id]}
                  onchange={() => onEnumDraftChange(setting)}
                  disabled={pictureSettingsBlocked || settingPending[setting.id]}
                  aria-label={setting.label}
                >
                  {#each COLOR_GAMUT_SETTING_OPTIONS as option (option.value)}
                    <option value={option.value}>{option.label}</option>
                  {/each}
                </select>
              {:else}
                <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
                  <input
                    type="range"
                    class="slider-sm"
                    min={setting.min}
                    max={setting.max}
                    step="1"
                    value={sliderDraftValue(setting)}
                    oninput={(event) => onNumericSliderInput(setting, event.currentTarget.value)}
                    onchange={() => commitNumericSlider(setting)}
                    style="--fill:{settingFillPercent(setting)}%"
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label={`${setting.label} slider`}
                  />
                </div>
                <div class="tv-setting-number-control">
                  <button
                    type="button"
                    class="tv-setting-number-step"
                    onclick={() => stepNumericSetting(setting, -1)}
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label={`Decrease ${setting.label}`}
                    title={`Decrease ${setting.label}`}
                  >−</button>
                  <input
                    type="number"
                    class="tv-setting-number"
                    min={setting.min}
                    max={setting.max}
                    step="1"
                    inputmode="numeric"
                    value={settingDraftValue(setting)}
                    oninput={(event) => onNumericSliderInput(setting, event.currentTarget.value)}
                    onchange={() => commitNumericSlider(setting)}
                    onkeydown={(event) => {
                      if (event.key !== 'Enter') return;
                      event.preventDefault();
                      commitNumericSlider(setting);
                    }}
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label={`${setting.label} exact value`}
                  />
                  <button
                    type="button"
                    class="tv-setting-number-step"
                    onclick={() => stepNumericSetting(setting, 1)}
                    disabled={pictureSettingsBlocked || settingPending[setting.id]}
                    aria-label={`Increase ${setting.label}`}
                    title={`Increase ${setting.label}`}
                  >+</button>
                </div>
              {/if}
              {#if settingPending[setting.id]}
                <span class="tv-setting-pending">Applying…</span>
              {/if}
            </div>
          {/each}
        </div>
        {#if activeSettingError}
          <p class="gpu-error tv-setting-error" role="alert">{activeSettingError}</p>
        {/if}
        {#if status.pictureSettingsError}
          <p class="tv-pairing-note">{status.pictureSettingsError}</p>
        {/if}
      {/if}
    </div>
  {/if}
</div>
