// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-daemon — transport-agnostic service (`api`) and the Unix-socket
//! NDJSON daemon with fs watching (`daemon`).

pub mod api;
pub mod daemon;
pub mod evaluate;
pub mod evidence;
pub mod recall_service;

pub use api::{Request, Response, ServeError, Service};
pub use daemon::{pid_path, run, socket_path};
pub use recall_service::RecallService;
