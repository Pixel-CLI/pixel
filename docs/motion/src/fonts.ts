// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {cancelRender, continueRender, delayRender, staticFile} from 'remotion';

// The site's three faces, served from public/fonts (latin subsets from
// Google Fonts, all under the SIL Open Font License). Every frame waits for
// them, so no render ever falls back to a system face.
const faces: [string, string, FontFaceDescriptors][] = [
	['Handjet', 'fonts/handjet.woff2', {weight: '100 900'}],
	['Archivo', 'fonts/archivo.woff2', {weight: '100 900'}],
	['IBM Plex Mono', 'fonts/plex-mono-400.woff2', {weight: '400'}],
	['IBM Plex Mono', 'fonts/plex-mono-500.woff2', {weight: '500'}],
	['IBM Plex Mono', 'fonts/plex-mono-600.woff2', {weight: '600'}],
];

const handle = delayRender('Loading fonts');
Promise.all(
	faces.map(([family, file, desc]) => {
		const face = new FontFace(family, `url(${staticFile(file)}) format('woff2')`, desc);
		document.fonts.add(face);
		return face.load();
	}),
)
	.then(() => continueRender(handle))
	.catch((err: unknown) => cancelRender(err));
