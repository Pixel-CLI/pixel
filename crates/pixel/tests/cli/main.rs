//! Single integration-test binary for the CLI crate. Each former
//! `tests/<name>.rs` is a module here, so cargo links one executable
//! instead of one per file (linking dominated `cargo test -p pixel-cli`).

// Compiled only with the `readify` feature, matching the gated command (issue #602).
#[cfg(feature = "readify")]
mod ai_cli_readify_cli;
mod ask_contract;
mod audit_cli;
mod classify_cli;
mod config_cli;
mod docs_drift;
mod doctor_cli;
mod evaluate_cli;
mod execution_brief;
#[cfg(unix)]
mod flow_cli;
mod guard_deny;
mod guard_enforce;
mod install_exit;
mod json_contract;
mod list_errors_cli;
mod metrics_cli;
mod post_edit_cli;
mod publish_cli;
mod recall_cli;
mod release_check_cli;
mod rename_cli;
mod renamed_commands;
mod rescue_cli;
mod run_recipe_cli;
mod scope_task_precision;
mod search_compat_cli;
mod space_cli;
mod support;
mod targets_cli;
mod task_cli;
mod task_route_cli;
mod task_watchdog;
mod ultraflow_cli;
mod uninstall_cli;
#[cfg(unix)]
mod update_notice_cli;
mod upgrade_cli;
mod version_cli;
mod web_search_cli;
