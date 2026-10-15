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

const ffmpeg = Bun.spawnSync([
	'ffmpeg',
	'-loglevel', 'error',
	'-y',
	'-ss', '8',
	'-i', source,
	'-t', '118.125',
	'-an',
	'-vf', 'setpts=PTS/6,scale=1600:1000:force_original_aspect_ratio=decrease:flags=lanczos,pad=1600:1000:(ow-iw)/2:(oh-ih)/2:color=0x18253b,fps=15,tpad=stop_mode=clone:stop_duration=1.5',
	'-c:v', 'libx264',
	'-crf', '10',
	'-pix_fmt', 'yuv420p',
	'-movflags', '+faststart',
	output,
], {stdout: 'inherit', stderr: 'inherit'});

if (ffmpeg.exitCode !== 0) {
	throw new Error(`ffmpeg failed with exit code ${ffmpeg.exitCode}`);
}

console.log(`Rendered the source recording from 00:08 to 02:06.125 at 6× speed, then held the final frame for 1.5s: ${output}`);
