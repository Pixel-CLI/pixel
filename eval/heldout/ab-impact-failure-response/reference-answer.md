`failure_response` is defined in `crates/pixel-daemon/src/api.rs:185`.

Production call sites:
- `crates/pixel-daemon/src/api.rs:925` (`Service::handle_inner`)
- `crates/pixel-daemon/src/daemon.rs:639, 651, 658, 676` (`handle_conn`)
- `crates/pixel-daemon/src/recall_service.rs:493, 495` (`RecallService::handle`)
- `crates/pixel/src/main.rs:2308` (`write_failure_envelope`)

Tests:
- `crates/pixel-daemon/src/api.rs:5175`
- `crates/pixel-daemon/src/daemon.rs:767, 791, 805, 885`
- `crates/pixel/src/recall_cmd.rs:1835` (unit test module)
- `crates/pixel/tests/cli/recall_cli.rs:167`
