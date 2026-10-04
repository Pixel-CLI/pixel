A global stdout cap in `crates/pixel/src/main.rs`: `STDOUT_BYTE_CAP` = 256 * 1024 bytes (256 KB), applied by `print_data` through `stdout_byte_cap()` / `render_data`. `PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides it and `0` lifts the cap entirely.

A `--json` answer over the cap is cut structurally by `truncate_structurally`: the largest arrays are shortened, other fields survive, and the object gains `truncated: true`, `cap_bytes` and `truncated_arrays` (path, kept, total). If no array trimming fits, it falls back to a `{truncated, cap_bytes, note, partial}` wrapper.
