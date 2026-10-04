`PROTOCOL_VERSION` is 13. It is defined in `crates/pixel-daemon/src/api.rs:43` (`pub const PROTOCOL_VERSION: u64 = 13;`).

The CLI (`crates/pixel/src/main.rs`) pings the daemon socket and `classify_ping` turns the answer into a `DaemonProbe`:
- an older protocol, no protocol, or a failed ping is `Stale`: `retire_stale_daemon_within` sends Shutdown and waits up to 2 s, then a fresh daemon is started;
- a newer protocol is `DaemonProbe::Newer`: the route is `Declined`, the newer daemon is left running and the command is served in process (`Service::open`).
