#!/usr/bin/env node
// Bumps the trailing numeric segment of the app version in every config file
// that carries it, and prints the new version to stdout.
//
//   0.8.0-alpha.7 -> 0.8.0-alpha.8     (pre-release: bump the counter)
//   0.8.0-alpha   -> 0.8.0-alpha.1     (pre-release without a counter yet)
//   0.7.3         -> 0.7.4             (stable: bump the patch)
//
// The channel word (alpha/beta/rc) and the major.minor.patch base are managed
// by hand; CI only ever advances the last number. Run by the `version-bump`
// job on every branch push, committed back by the workflow.
import { readFileSync, writeFileSync } from 'node:fs';

const PKG = 'package.json';
const TAURI_CONF = 'src-tauri/tauri.conf.json';
const CARGO_TOML = 'src-tauri/Cargo.toml';
const CARGO_LOCK = 'src-tauri/Cargo.lock';

const current = JSON.parse(readFileSync(PKG, 'utf8')).version;
const m = current.match(/^(\d+)\.(\d+)\.(\d+)(?:-(.+))?$/);
if (!m) throw new Error(`unparsable version in ${PKG}: ${current}`);

let next;
if (m[4] !== undefined) {
  const tail = m[4].match(/^(.*?)(\d+)$/); // trailing number of the pre-release part
  next = tail
    ? `${m[1]}.${m[2]}.${m[3]}-${tail[1]}${Number(tail[2]) + 1}`
    : `${m[1]}.${m[2]}.${m[3]}-${m[4]}.1`;
} else {
  next = `${m[1]}.${m[2]}.${Number(m[3]) + 1}`;
}

// Targeted string replacement keeps each file's formatting untouched.
const replaceOnce = (file, re, replacement) => {
  const text = readFileSync(file, 'utf8');
  if (!re.test(text)) throw new Error(`${file}: version pattern not found`);
  writeFileSync(file, text.replace(re, replacement));
};

const esc = current.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
replaceOnce(PKG, new RegExp(`("version":\\s*")${esc}(")`), `$1${next}$2`);
replaceOnce(TAURI_CONF, new RegExp(`("version":\\s*")${esc}(")`), `$1${next}$2`);
replaceOnce(CARGO_TOML, new RegExp(`(^version = ")${esc}(")`, 'm'), `$1${next}$2`);
replaceOnce(
  CARGO_LOCK,
  new RegExp(`(name = "tacho-bridge-application"\\nversion = ")${esc}(")`),
  `$1${next}$2`,
);

console.log(next);
