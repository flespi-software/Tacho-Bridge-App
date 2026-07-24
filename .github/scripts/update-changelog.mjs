#!/usr/bin/env node
// Appends a CHANGELOG.md section for the version being released, generated
// from the commit subjects since the previous CI version bump:
//
//   ### [0.8.0-alpha.8] - 2026-07-24
//
//   🛠 Fixes
//
//   Fixed rack detection on Windows.
//
//   🆕 Features / Improvements
//
//   Added the pre-release update channel.
//
// Commit subjects starting with fix/fixed/bug/hotfix go under Fixes;
// everything else goes under Features / Improvements.
// Usage: node update-changelog.mjs <new-version>   (run from the repo root,
// requires full git history — checkout with fetch-depth: 0)
import { readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';

const CHANGELOG = 'CHANGELOG.md';
const version = process.argv[2];
if (!version) throw new Error('usage: update-changelog.mjs <new-version>');

const git = (cmd) => execSync(`git ${cmd}`, { encoding: 'utf8' }).trim();

// Everything the user pushed since the previous bot bump. On the very first
// run there is no previous bump — fall back to the head commit alone.
const prevBump = git(`log --grep "^ci: version " -1 --format=%H || true`);
const range = prevBump ? `${prevBump}..HEAD` : '-1';
const subjects = git(`log ${range} --no-merges --format=%s`)
  .split('\n')
  .map((s) => s.trim())
  .filter((s) => s.length > 0 && !s.startsWith('ci: version'))
  .reverse(); // git log is newest-first; the changelog reads chronologically

const ensureDot = (s) => (/[.!?]$/.test(s) ? s : `${s}.`);
// Two trailing spaces: markdown line break, matches the existing file style.
const line = (s) => `${ensureDot(s)}  `;

const fixes = [];
const features = [];
for (const s of subjects) {
  (/^(fix|fixed|bug|hotfix)/i.test(s) ? fixes : features).push(line(s));
}
if (fixes.length === 0 && features.length === 0) features.push(line('Maintenance build'));

const date = new Date().toISOString().slice(0, 10);
let body = '';
if (fixes.length > 0) body += `🛠 Fixes\n\n${fixes.join('\n')}\n`;
if (features.length > 0) {
  if (body) body += '\n';
  body += `🆕 Features / Improvements\n\n${features.join('\n')}\n`;
}

// The file is oldest-first, so the new section goes to the end.
const current = readFileSync(CHANGELOG, 'utf8').replace(/\s*$/, '\n');
const section = `\n### [${version}] - ${date}\n\n${body}`;
writeFileSync(CHANGELOG, current + section);

console.log(section.trim());
