# Pixel — Slack Announcement Card

## Static Card (Markdown for Slack)

```
┌─────────────────────────────────────────────┐
│                                             │
│  🤖 Pixel — AI-Native Development Tool     │
│                                             │
│  Pixel is a local code index that helps     │
│  AI agents read less and code more.         │
│                                             │
│  ┌─────────────────────────────────────┐    │
│  │  ✅ Local classification            │    │
│  │  ✅ Graph answers with epistemics   │    │
│  │  ✅ Agent hooks                     │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  📦 Install: curl -fsSL https://raw.githubusercontent.com/Pixel-CLI/pixel/main/install.sh | sh │
│  📖 Docs: https://pixel-cli.dev/docs      │
│  💬 Slack: #pixel-dev                      │
│                                             │
└─────────────────────────────────────────────┘
```

## Verifiable Evidence

| Claim | Evidence | Verification |
|-------|----------|--------------|
| Non-deterministic results disclosed | `pixel classify --json` shows `deterministic` and `provider` fields | `pixel classify --json` |
| Graph answers with epistemics | `complete`, `capped`, `unresolved` markers | `pixel list-signatures` |
| Agent tool-call guard | `pixel hook guard` enforces tool-call boundaries | `pixel hook guard --provider claude` |

## Key Features

- **Local**: The index lives in `.pixel/` on your machine; no account, no API key
- **Classification**: `pixel classify` discloses when results are model-backed vs deterministic
- **Graph answers**: Every answer carries `complete`, `capped`, or `unresolved` markers
- **Agent hooks**: `pixel install` adds optional `pixel-classify` skill for configured harnesses

## Call to Action

- Try Pixel: `curl -fsSL https://raw.githubusercontent.com/Pixel-CLI/pixel/main/install.sh | sh`
- Read the docs: https://pixel-cli.dev/docs
- Join the community: #pixel-dev on Slack
