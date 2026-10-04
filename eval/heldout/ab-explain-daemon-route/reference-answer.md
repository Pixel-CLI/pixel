1. `main.rs` (`crates/pixel/src/main.rs`) parses the command and calls `execute`, which discovers the repo root and calls `route_through_daemon`.
2. It probes the socket with a ping. No daemon answers, so `auto_start_daemon` spawns `pixel daemon start <root> --foreground` in the background and polls the socket for up to five seconds, sending the request once it answers.
3. If the start is disabled or times out, the CLI opens `Service::open` in-process and handles the request directly; both paths return the same Envelope.
4. Turn it off with `PIXEL_DAEMON_AUTO_START=0` or `daemon_auto_start: false` in the config.
