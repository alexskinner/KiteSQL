// Copyright 2024 KipData/KiteSQL
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! One source, two paths: an item written once as `async` and marked `#[maybe_async]` is compiled as
//! the sync path by default and as the async path with the `async` feature, under the same
//! names. See `kite_sql_serde_macros::maybe_async` for the rules.
//!
//! `MaybeSend` / `MaybeSync` / `MaybeStatic` hold for every type on the sync path and mean
//! `Send` / `Sync` / `'static` on the async path.

#[cfg(not(feature = "async"))]
mod bounds {
    /// `Send` on the async path; implemented by every type on the sync path.
    pub trait MaybeSend {}
    impl<T: ?Sized> MaybeSend for T {}

    /// `Sync` on the async path; implemented by every type on the sync path.
    pub trait MaybeSync {}
    impl<T: ?Sized> MaybeSync for T {}

    /// `'static` on the async path; implemented by every type on the sync path.
    pub trait MaybeStatic {}
    impl<T: ?Sized> MaybeStatic for T {}
}

#[cfg(feature = "async")]
mod bounds {
    pub use core::marker::{Send as MaybeSend, Sync as MaybeSync};

    /// Async transactions must be `'static`: the compiler loses the implied `T: 't` bounds when
    /// it checks that a future holding `&'t T` is `Send`, and `T: 'static` restores them for
    /// every `'t`.
    pub trait MaybeStatic: 'static {}
    impl<T: ?Sized + 'static> MaybeStatic for T {}
}

pub use bounds::{MaybeSend, MaybeStatic, MaybeSync};

/// Runs a test future to completion; on the sync path `#[maybe_async]` has already turned the
/// `async` block into its value, so this is the identity.
#[cfg(all(test, not(feature = "async")))]
pub(crate) fn block_on<T>(value: T) -> T {
    value
}

#[cfg(all(test, feature = "async"))]
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    // The future runs on this thread; the storage's background tasks get worker threads, so
    // they keep going while a test blocks this thread (e.g. joining a connection thread).
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to start a tokio runtime")
        .block_on(future)
}
