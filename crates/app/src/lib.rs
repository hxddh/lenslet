//! Lenslet: a local AWS account in a window. It embeds AWS Emulator, serves
//! it on one local endpoint, runs your commands in a terminal pointed at it,
//! and shows every call, what exists, and the IAM permissions used.

pub mod account;
pub mod copy_as;
pub mod emulator;
pub mod engine;
pub mod functions;
pub mod gaps;
pub mod observe;
pub mod replay;
pub mod resources;
pub mod server;
pub mod session;
pub mod shell;
mod ui;

pub use ui::App;

use nsh_core::prelude::*;

#[native_module("lenslet/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}
