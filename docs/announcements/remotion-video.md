# Pixel — Remotion Video Script

## Video: "Pixel in 30 Seconds"

### Scene 1: Problem (0-5s)

**Visual**: Developer struggling with multiple AI tools, context switching between terminals.

**Text overlay**: "AI development tools are fragmented. Context is lost. Results are inconsistent."

**Audio**: Upbeat, energetic music starts.

### Scene 2: Solution (5-15s)

**Visual**: Pixel logo animates in. Terminal shows `pixel classify --json`.

**Text overlay**: "Pixel: structured, verifiable AI development. One tool. Transparent results."

**Code shown**:
```bash
$ pixel classify "fix the login bug" --json
{
  "predicted": "bugfix",
  "probs": { "bugfix": 0.95, "feature": 0.03 },
  "snapshot": { "deterministic": true, "provider": "ollaya" }
}
```

### Scene 3: Features (15-25s)

**Visual**: Split screen showing Pixel's key features.

**Text overlay**: "Local classification. Graph answers. Agent hooks."

**Code shown**:
```bash
$ pixel classify "refactor auth" --labels bugfix --labels feature --engine ollaya
$ pixel list-signatures src/main.rs
$ pixel hook guard --provider claude
```

### Scene 4: Call to Action (25-30s)

**Visual**: Pixel logo with install command.

**Text overlay**: "Install Pixel today."

**Code shown**:
```bash
$ curl -fsSL https://raw.githubusercontent.com/Pixel-CLI/pixel/main/install.sh | sh
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
- `pixel list-signatures` — see `crates/pixel/src/list_signatures.rs`
- `pixel hook guard` — see `crates/pixel/src/guard.rs`
