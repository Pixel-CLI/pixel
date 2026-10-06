# Pixel — Remotion Video Script

## Video: "Pixel in 30 Seconds"

### Scene 1: Problem (0-5s)

**Visual**: Developer struggling with multiple AI tools, context switching between terminals.

**Text overlay**: "AI development tools are fragmented. Context is lost. Results are inconsistent."

**Audio**: Upbeat, energetic music starts.

### Scene 2: Solution (5-15s)

**Visual**: Pixel logo animates in. Terminal shows `pixel classify --json`.

**Text overlay**: "Pixel: deterministic AI development. One tool. Verifiable results."

**Code shown**:
```bash
$ pixel classify "fix the login bug" --json
{
  "predicted": "bugfix",
  "probs": { "bugfix": 0.95, "feature": 0.03 },
  "deterministic": false,
  "provider": "local"
}
```

### Scene 3: Features (15-25s)

**Visual**: Split screen showing Pixel's key features.

**Text overlay**: "Verified history. Structural analysis. Cross-agent guidance."

**Code shown**:
```bash
$ pixel classify-history list
# Human-verified labels improve accuracy

$ pixel structural --check
# Pre-review checks catch issues early

$ pixel guard --check
# Cross-agent guidance for Claude, Cursor, Codex
```

### Scene 4: Call to Action (25-30s)

**Visual**: Pixel logo with install command.

**Text overlay**: "Install Pixel today."

**Code shown**:
```bash
$ curl -fsSL https://pixel.dev | sh
```

**Audio**: Music fades out.

## Production Notes

- **Duration**: 30 seconds
- **Format**: 1080x1080 (square for social media)
- **Style**: Clean, modern, terminal-focused
- **Branding**: Pixel logo, monospace font, dark theme

## Remotion Project Structure

```
remotion-pixel/
├── src/
│   ├── Root.tsx           # Main composition
│   ├── scenes/
│   │   ├── Problem.tsx    # Scene 1: Problem
│   │   ├── Solution.tsx   # Scene 2: Solution
│   │   ├── Features.tsx   # Scene 3: Features
│   │   └── CallToAction.tsx # Scene 4: CTA
│   └── components/
│       ├── Terminal.tsx   # Terminal component
│       └── CodeBlock.tsx  # Code block component
├── package.json
└── README.md
```

## Verification

All code examples in the video are verifiable:

- `pixel classify --json` — see `crates/pixel/src/classify.rs`
- `pixel classify-history list` — see `crates/pixel/src/classify_history.rs`
- `pixel structural --check` — see `crates/pixel/src/structural.rs`
- `pixel guard --check` — see `crates/pixel/src/guard.rs`
