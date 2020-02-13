// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

#![feature(box_patterns)]
// TODO remove following allows.
#![allow(dead_code)]
#![allow(unused_imports)]

#[macro_use]
extern crate slog_global;
#[macro_use]
extern crate failure;

mod delegate;
mod endpoint;
mod errors;
mod observer;
mod service;

pub use endpoint::{Endpoint, Task};
pub use errors::{Error, Result};
pub use observer::CdcObserver;
pub use service::Service;
