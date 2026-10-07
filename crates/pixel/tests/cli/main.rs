// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for the CLI crate. Each former
//! `tests/<name>.rs` is a module here, so cargo links one executable
//! instead of one per file (linking dominated `cargo test -p pixel-cli`).

// Compiled only with the `readify` feature, matching the gated command (issue #602).
#[cfg(feature = "readify")]
mod ai_cli_readify_cli;
mod ask_contract;
mod audit_cli;
mod audit_prompt;
mod classify_cli;
mod config_cli;
mod cycles_cli;
mod docs_drift;
mod doctor_cli;
mod evaluate_cli;
mod execution_brief;
#[cfg(unix)]
mod flow_cli;
mod hook_stdin_cap_cli;
mod impact_read_cli;
mod install_exit;
mod json_contract;
mod list_errors_cli;
mod metrics_cli;
mod ollaya_fresh_install_cli;
mod prompt_brief_cli;
mod publish_cli;
mod recall_cli;
mod reference_cli;
mod release_check_cli;
mod rename_cli;
mod renamed_commands;
mod rescue_cli;
mod run_recipe_cli;
mod scope_task_precision;
mod search_compat_cli;
mod sidecar_trust_cli;
mod space_cli;
mod support;
mod targets_cli;
mod task_cli;
mod task_route_cli;
mod task_structural_cli;
mod task_watchdog;
mod ultraflow_cli;
mod uninstall_cli;
#[cfg(unix)]
mod update_notice_cli;
mod upgrade_cli;
mod version_cli;
mod web_search_cli;
