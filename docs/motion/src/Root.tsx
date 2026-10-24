// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

import {Composition} from 'remotion';
import {EditGuard, PIXELvsRTK} from './Turns';

const comps = [
	['PIXELvsRTK', PIXELvsRTK],
	['EditGuard', EditGuard],
] as const;

export const RemotionRoot = () => (
	<>
		{comps.map(([id, component]) => (
			<Composition
				key={id}
				id={id}
				component={component}
				durationInFrames={240}
				fps={30}
				width={1600}
				height={1000}
			/>
		))}
	</>
);
