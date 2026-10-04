#!/usr/bin/env node
// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// WarpGrep arm for bench-retrieval.py: one search, JSON on stdout.
// usage: MORPH_API_KEY=... MORPH_SDK_DIR=<dir> node warpgrep-search.mjs "<query>" <repoRoot>
// MORPH_SDK_DIR is where `npm install @morphllm/morphsdk` ran; the SDK stays
// out of this repo's dependencies. The SDK runs WarpGrep's tools locally with
// ripgrep, but their results (grep lines, file reads) are sent to Morph's API.
// Exit 1 on a failed search so the harness records a failure, not a miss.
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';

const sdk = join(process.env.MORPH_SDK_DIR ?? '.', 'node_modules/@morphllm/morphsdk/dist/index.js');
const { MorphClient } = await import(pathToFileURL(sdk).href);

const [query, repoRoot] = process.argv.slice(2);
const morph = new MorphClient({ apiKey: process.env.MORPH_API_KEY });
const t0 = performance.now();
let turns = 0, toolCalls = 0;
const stream = morph.warpGrep.execute({ searchTerm: query, repoRoot, streamSteps: true });
let step = await stream.next();
while (!step.done) {
  turns += 1;
  toolCalls += (step.value.toolCalls ?? []).length;
  step = await stream.next();
}
const r = step.value;
const out = {
  success: r.success,
  error: r.error ?? null,
  ms: Math.round(performance.now() - t0),
  turns,
  tool_calls: toolCalls,
  results: (r.contexts ?? []).map(c => ({ file_path: c.file, lines: c.lines ?? null, bytes: Buffer.byteLength(c.content, 'utf8') })),
  content_bytes: (r.contexts ?? []).reduce((n, c) => n + Buffer.byteLength(c.content, 'utf8'), 0),
};
process.stdout.write(JSON.stringify(out));
process.exit(r.success ? 0 : 1);
