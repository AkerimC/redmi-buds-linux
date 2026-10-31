//! Control Redmi/Xiaomi buds (JieLi RCSP derivative) over Bluetooth RFCOMM.
//!
//! Zero dependencies: the RFCOMM socket is opened through raw `AF_BLUETOOTH`
//! FFI, and the auth transform is a native port (see [`auth`]).

pub mod auth;
pub mod bt;
pub mod client;
pub mod rcsp;
pub mod settings;
