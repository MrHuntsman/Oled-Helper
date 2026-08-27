// Generates latest.json for the portable updater from the built release exe.
// Run by the GitHub Actions release workflow after `npm run tauri build`.
//
// Usage: node scripts/make-latest-json.mjs [expected-version]
//   expected-version is the git tag without the "v" prefix (e.g. 1.0.0). When
//   given, the build fails if it does not match the version in
//   tauri.conf.json, so a release can never ship a manifest whose version
//   does not match the exe actually built.
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';

const REPO = 'MrHuntsman/Oled-Helper';
const EXE_RELATIVE_PATH = 'src-tauri/target/release/OledHelper.exe';

const conf = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
const version = String(conf.version || '').trim();
if (!version) {
  console.error('latest.json: missing "version" in src-tauri/tauri.conf.json');
  process.exit(1);
}

const expected = process.argv[2];
if (expected && expected !== version) {
  console.error(
    `latest.json: tag version "${expected}" does not match tauri.conf.json version "${version}". ` +
      'Bump the version in src-tauri/tauri.conf.json (and src-tauri/Cargo.toml) to match the tag before releasing.'
  );
  process.exit(1);
}

const data = readFileSync(EXE_RELATIVE_PATH);
const sha256 = createHash('sha256').update(data).digest('hex');
const manifest = {
  version,
  url: `https://github.com/${REPO}/releases/latest/download/OledHelper.exe`,
  sha256,
};
writeFileSync('latest.json', `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`latest.json written: v${version} (${data.length} bytes, sha256 ${sha256.slice(0, 12)}...)`);
