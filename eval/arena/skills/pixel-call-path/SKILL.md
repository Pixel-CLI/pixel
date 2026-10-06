---
name: pixel-call-path
description: Trace a named code entry to a named destination when the request asks how execution reaches that point; exclude generic explanations, text searches, and edits.
---

# Trace a named call path

Use only when the request identifies both endpoints, such as a route handler and a downstream function. If either endpoint is unclear, use native source search.

Run one query from the repository root. For ambiguous names, a Pixel UID is `<relative-file>#<qualified-name>#<kind>`, where kind is `function` or `method`:

```sh
pixel --metrics off call-path '<entry-uid>' '<destination-uid>' --json .
```

Treat the chain as a navigation lead. Open each source location and confirm every internal call edge; inspect branch conditions when the question needs them. If results are ambiguous or unhelpful, stop querying and use native search/source reading. A missing path is not proof that no source path exists.

Do not run install, build-index, or other maintenance commands to answer. This experiment assumes a prepared graph; call-path may ask Pixel's daemon to ensure it, so if the graph is missing or gets built/updated, stop using Pixel and fall back to native tools.
