//! [zero] @claude The threaded wasm build of librustzcash.
//!
//! The library half is the one export a page needs before anything parallel runs:
//! `initThreadPool(n)`, which starts `n` Web Workers sharing this module's memory and
//! makes them rayon's global pool. Everything else is in `tests/`.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

pub use wasm_bindgen_rayon::init_thread_pool;
