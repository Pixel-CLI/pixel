// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for the CLI crate. Each former
//! `tests/<name>.rs` is a module here, so cargo links one executable
//! instead of one per file (linking dominated `cargo test -p pixel-cli`).

mod ask_contract;
mod audit_cli;
mod audit_prompt;
mod brief_feedback_cli;
mod bundled_brief_cli;
mod classify_cli;
mod classify_eval_cli;
mod config_cli;
mod core_commands_cli;
mod cycles_cli;
mod docs_drift;
mod doctor_cli;
mod evaluate_cli;
mod execution_brief;
mod french_brief_cli;
mod graph_shadow_cli;
mod hook_guarantees_cli;
#[cfg(unix)]
mod hook_stdin_cap_cli;
mod impact_read_cli;
mod install_exit;
mod json_contract;
mod metrics_cli;
mod ollaya_fresh_install_cli;
#[cfg(unix)]
mod plugin_cli;
mod prompt_brief_cli;
mod reference_cli;
mod region_pack_cli;
mod regions_cli;
mod release_check_cli;
mod rename_cli;
mod run_recipe_cli;
mod scope_task_precision;
mod search_compat_cli;
mod setup_cli;
#[cfg(unix)]
mod setup_goldens_cli;
mod sidecar_trust_cli;
mod support;
mod targets_cli;
mod task_cli;
mod task_route_cli;
mod task_structural_cli;
mod task_watchdog;
mod uninstall_cli;
#[cfg(unix)]
mod update_notice_cli;
mod upgrade_cli;
mod vendored_boundary;
mod version_cli;
mod words_brief_cli;
