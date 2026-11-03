// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {mkdir} from 'node:fs/promises';
import {dirname, resolve} from 'node:path';

const [sourceArg, outputArg] = process.argv.slice(2);
if (!sourceArg || !outputArg) {
	throw new Error('Usage: bun docs/motion/prompt-submit/render-recording.ts <source.mp4> <output.mp4>');
}

const source = resolve(sourceArg);
const output = resolve(outputArg);
await mkdir(dirname(output), {recursive: true});

// The recording holds two CLI panes side by side. Crop each pane to its left
// half (half the terminal columns) and put the crops side by side, so the
// README — which shows the pair across its full column width — renders each
// glyph about twice as large instead of squeezing ~80 columns into each half.
const filter = [
	'[0:v]setpts=PTS/6,fps=15,tpad=stop_mode=clone:stop_duration=1.5,split=2[base][base2]',
	'[base]crop=iw/4:ih:0:0,scale=400:1000:force_original_aspect_ratio=decrease:flags=lanczos,setsar=1,pad=400:1000:(ow-iw)/2:(oh-ih)/2:color=0x18253b[left]',
	'[base2]crop=iw/4:ih:iw/2:0,scale=400:1000:force_original_aspect_ratio=decrease:flags=lanczos,setsar=1,pad=400:1000:(ow-iw)/2:(oh-ih)/2:color=0x18253b[right]',
	'[left][right]hstack=inputs=2,format=yuv420p[out]',
].join(';');

const ffmpeg = Bun.spawnSync([
	'ffmpeg',
	'-loglevel', 'error',
	'-y',
	'-ss', '8',
	'-i', source,
	'-t', '118.125',
	'-an',
	'-filter_complex', filter,
	'-map', '[out]',
	'-c:v', 'libx264',
	'-crf', '10',
	'-pix_fmt', 'yuv420p',
	'-movflags', '+faststart',
	output,
], {stdout: 'inherit', stderr: 'inherit'});

if (ffmpeg.exitCode !== 0) {
	throw new Error(`ffmpeg failed with exit code ${ffmpeg.exitCode}`);
}

console.log(`Rendered the source recording from 00:08 to 02:06.125 at 6× speed, cropped each pane to its left half and put them side by side (800×1000), then held the final frame for 1.5s: ${output}`);
