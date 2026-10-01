export function updatePictureSettingDrafts({
  currentMode,
  nextMode,
  currentSettings,
  nextSettings,
  drafts,
  inFlightWrites,
  queuedSettingWrites,
}) {
  const modeChanged = currentMode !== nextMode;
  const previousById = new Map(currentSettings.map((setting) => [setting.id, setting.value]));
  let tvValueChanged = false;

  for (const setting of nextSettings) {
    if (
      inFlightWrites[setting.id]
      || queuedSettingWrites[setting.id]
    ) continue;
    if (
      modeChanged
      || drafts[setting.id] === undefined
      || previousById.get(setting.id) !== setting.value
    ) {
      drafts[setting.id] = setting.value ?? '';
      tvValueChanged = true;
    }
  }

  return tvValueChanged;
}

export function enumSettingValue(setting, drafts) {
  const draft = drafts[setting.id];
  return draft === undefined || draft === '' ? (setting.value ?? '') : String(draft);
}

export function enumSettingOptions(setting, options, drafts) {
  const value = enumSettingValue(setting, drafts);
  if (!value || options.some((option) => option.value === value)) return options;
  return [{ value, label: value }, ...options];
}
