// SPDX-License-Identifier: GPL-3.0-or-later

pub mod binary;
pub mod cloud;
pub mod credentials;
pub mod engine;
pub mod local;
pub mod machines;
pub mod model;
pub mod store;

#[cfg(feature = "desktop")]
mod desktop;

#[cfg(feature = "desktop")]
pub use desktop::run;
