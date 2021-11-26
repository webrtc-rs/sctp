//! Low-level protocol logic for the SCTP protoocol
//!
//! sctp-proto contains a fully deterministic implementation of SCTP protocol logic. It contains
//! no networking code and does not get any relevant timestamps from the operating system. Most
//! users may want to use the futures-based sctp API instead.
//!
//! The sctp-proto API might be of interest if you want to use it from a C or C++ project
//! through C bindings or if you want to use a different event loop than the one tokio provides.
//!
//! The most important types are `Endpoint`, which conceptually represents the protocol state for
//! a single socket and mostly manages configuration and dispatches incoming datagrams to the
//! related `Association`. `Association` types contain the bulk of the protocol logic related to
//! managing a single association and all the related state (such as streams).

#![warn(rust_2018_idioms)]
#![allow(dead_code)]

pub mod param;
pub mod chunk;
pub mod error;
pub mod error_cause;
pub mod packet;

pub(crate) mod util;
