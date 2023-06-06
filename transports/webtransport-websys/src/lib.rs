//! Libp2p WebTransport built on [web-sys](https://rustwasm.github.io/wasm-bindgen/web-sys/index.html)
//!
//! This crate uses some unstable apis of web-sys so it requires
//! `--cfg=web_sys_unstable_apis` to be activated, as [described
//! in the `wasm-bindgen` guide](https://rustwasm.github.io/docs/wasm-bindgen/web-sys/unstable-apis.html)

mod bindings;
mod connection;
mod endpoint;
mod error;
mod fused_js_promise;
mod stream;
mod transport;
mod utils;

pub use self::connection::Connection;
pub use self::error::Error;
pub use self::stream::Stream;
pub use self::transport::{Config, Transport};

#[cfg(test)]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
