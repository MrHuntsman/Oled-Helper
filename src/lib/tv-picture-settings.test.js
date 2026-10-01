import assert from 'node:assert/strict';
import test from 'node:test';
import {
  enumSettingOptions,
  enumSettingValue,
  updatePictureSettingDrafts,
} from './tv-picture-settings.js';

function update({ currentMode, nextMode, currentSettings, nextSettings, drafts = {} }) {
  const changed = updatePictureSettingDrafts({
    currentMode,
    nextMode,
    currentSettings,
    nextSettings,
    drafts,
    inFlightWrites: {},
    queuedSettingWrites: {},
  });
  return { changed, drafts };
}

test('mode changes refresh enum drafts even when a setting value is unchanged', () => {
  const oldSettings = [
    { id: 'colorGamut', value: 'dynamic' },
    { id: 'hdrDynamicToneMapping', value: 'on' },
  ];
  const result = update({
    currentMode: 'hdrCinema',
    nextMode: 'hdrCinemaBright',
    currentSettings: oldSettings,
    nextSettings: oldSettings,
  });

  assert.equal(result.changed, true);
  assert.equal(result.drafts.colorGamut, 'dynamic');
  assert.equal(result.drafts.hdrDynamicToneMapping, 'on');
});

test('an initial setting snapshot initializes enum drafts', () => {
  const result = update({
    currentMode: null,
    nextMode: 'hdrCinemaBright',
    currentSettings: [],
    nextSettings: [{ id: 'colorGamut', value: 'native' }],
  });

  assert.equal(result.drafts.colorGamut, 'native');
  assert.equal(enumSettingValue({ id: 'colorGamut', value: 'native' }, result.drafts), 'native');
});

test('enum dropdown retains an unfamiliar TV value as a selectable option', () => {
  const setting = { id: 'colorGamut', value: 'wide' };
  const options = [{ value: 'auto', label: 'Auto' }, { value: 'native', label: 'Native' }];

  assert.equal(enumSettingValue(setting, {}), 'wide');
  assert.deepEqual(enumSettingOptions(setting, options, {}), [
    { value: 'wide', label: 'wide' },
    ...options,
  ]);
});
