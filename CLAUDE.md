# Project Rules

## Reinstall and Reconfig After Each Implementation Turn

After finishing any implementation turn in this repo (code edit + verify cycle), you MUST:

1. **Rebuild and reinstall the pixel binary** so the installed CLI matches the working tree:
   ```bash
   cargo build --release -p pixel-cli && cp target/release/pixel ~/.local/bin/pixel
   ```
2. **In parallel** (both only need the new binary, not each other):
   - **Track A:** `pixel index --history .` — rebuild the facts/history index.
   - **Track B:** `build-agent-config && pixel install` — propagate rule edits to tool directories, then reinstall hooks and managed blocks.
3. **Run `pixel doctor`** and confirm green (or explicitly report any non-green check).
