# Pixel — Slack Announcement Card

## Static Card (Markdown for Slack)

```
┌─────────────────────────────────────────────┐
│                                             │
│  🤖 Pixel — AI-Native Development Tool     │
│                                             │
│  Pixel is a deterministic AI development    │
│  tool that helps you build software faster. │
│                                             │
│  ┌─────────────────────────────────────┐    │
│  │  ✅ Deterministic classification     │    │
│  │  ✅ Verified-history retrieval       │    │
│  │  ✅ Structural analysis             │    │
│  │  ✅ Cross-agent guidance             │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  📦 Install: curl -fsSL https://pixel.dev  │
│  📖 Docs: https://pixel.dev/docs          │
│  💬 Slack: #pixel-dev                      │
│                                             │
└─────────────────────────────────────────────┘
```

## Verifiable Evidence

| Claim | Evidence | Verification |
|-------|----------|--------------|
| Deterministic classification | `snapshot.deterministic=false` disclosed | `pixel classify --json` |
| Verified-history retrieval | `crates/pixel/src/classify_history.rs` | `pixel classify-history list` |
| Structural analysis | `crates/pixel/src/structural.rs` | `pixel structural --check` |
| Cross-agent guidance | `crates/pixel/src/guard.rs` | `pixel guard --check` |

## Key Features

- **Deterministic**: Pixel discloses when results are model-backed vs deterministic
- **Verified-history**: Human-verified labels improve classification accuracy
- **Structural analysis**: Pre-review checks catch issues before human review
- **Cross-agent**: Works with Claude, Cursor, Codex, and other AI agents

## Call to Action

- Try Pixel: `curl -fsSL https://pixel.dev | sh`
- Read the docs: https://pixel.dev/docs
- Join the community: #pixel-dev on Slack
