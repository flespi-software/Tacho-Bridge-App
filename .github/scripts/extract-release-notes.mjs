#!/usr/bin/env node
// Prints the CHANGELOG.md section body for the given version — used as the
// GitHub release notes. The section grammar (`### [<version>] - <date>`)
// must match what update-changelog.mjs writes.
// Usage: node extract-release-notes.mjs <version> [outfile]
import { readFileSync, writeFileSync } from 'node:fs';

const version = process.argv[2];
const outfile = process.argv[3];
if (!version) throw new Error('usage: extract-release-notes.mjs <version> [outfile]');

const escapeRe = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const text = readFileSync('CHANGELOG.md', 'utf8');
const re = new RegExp(
  `### \\[${escapeRe(version)}\\][^\n]*\n([\\s\\S]*?)(?=\n### \\[|$)`,
);
const m = text.match(re);
const notes = m ? m[1].trim() + '\n' : '';
if (outfile) writeFileSync(outfile, notes);
process.stdout.write(notes);
