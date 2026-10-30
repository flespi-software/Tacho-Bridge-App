#!/usr/bin/env node
// Fallback for a known flaky Node/libuv crash on Windows: `quasar build`
// sometimes finishes the build successfully and then aborts on process exit
// ("Assertion failed: !(handle->flags & UV_HANDLE_CLOSING), src\win\async.c",
// exit 0xC0000409). Run as `quasar build || node verify-frontend-dist.mjs`:
// if a fresh build output exists despite the non-zero exit, treat the build
// as successful; otherwise fail for real.
import { statSync } from 'node:fs';

const ENTRY = 'dist/spa/index.html';
const MAX_AGE_MS = 15 * 60 * 1000; // stale output must not mask a real failure

let stat;
try {
  stat = statSync(ENTRY);
} catch {
  console.error(`verify-frontend-dist: ${ENTRY} does not exist — the build really failed.`);
  process.exit(1);
}

const age = Date.now() - stat.mtimeMs;
if (age > MAX_AGE_MS) {
  console.error(
    `verify-frontend-dist: ${ENTRY} is ${Math.round(age / 60000)} min old — ` +
      'stale output from an earlier build, the current build really failed.',
  );
  process.exit(1);
}

console.log(
  'verify-frontend-dist: build output is present and fresh — ' +
    'treating the non-zero exit as the known Windows libuv shutdown crash.',
);
