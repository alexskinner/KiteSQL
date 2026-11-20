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

//! Shared by the integration tests, so one test source runs on the sync and the async path,
//! on whichever storage the build has.
//!
//! A test file mounts it as `crate::maybe_async`, where `#[maybe_async]` looks for `block_on`:
//!
//! ```ignore
//! mod common;
//! use common as maybe_async;
//! use kite_sql_serde_macros::maybe_async;
//! ```
#![allow(dead_code, unused_imports)]

use kite_sql::db::{DataBaseBuilder, Database};
use kite_sql::errors::DatabaseError;
use kite_sql_serde_macros::maybe_async;
use std::path::Path;

/// Runs a test future to completion; on the sync path `#[maybe_async]` has already turned the
/// `async` block into its value, so this is the identity.
#[cfg(not(feature = "async"))]
pub fn block_on<T>(value: T) -> T {
    value
}

/// Runs a test future to completion on this thread; the storage's background tasks get worker
/// threads.
#[cfg(feature = "async")]
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to start a tokio runtime")
        .block_on(future)
}

/// The storage the tests run on: LMDB when the build has it, otherwise SlateDB on an in-memory
/// object store.
#[cfg(feature = "lmdb")]
pub type TestStorage = kite_sql::storage::lmdb::LmdbStorage;
#[cfg(all(not(feature = "lmdb"), feature = "slatedb"))]
pub type TestStorage = kite_sql::storage::slatedb::SlateDbStorage;

/// Opens the test storage: LMDB in `dir`, or SlateDB on a fresh in-memory object store.
#[maybe_async]
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
pub async fn test_storage(dir: &Path) -> Result<TestStorage, DatabaseError> {
    #[cfg(feature = "lmdb")]
    let storage = kite_sql::storage::lmdb::LmdbStorage::new(dir);
    #[cfg(all(not(feature = "lmdb"), feature = "slatedb"))]
    let storage = {
        let _ = dir;
        in_memory_slatedb().await
    };
    storage
}

#[cfg(feature = "slatedb")]
async fn in_memory_slatedb() -> Result<kite_sql::storage::slatedb::SlateDbStorage, DatabaseError> {
    use slatedb::object_store::memory::InMemory;
    use std::sync::Arc;

    Ok(kite_sql::storage::slatedb::SlateDbStorage::open(
        "kitesql-test",
        Arc::new(InMemory::new()),
        Default::default(),
    )
    .await?
    .await_durable(false))
}

/// Builds databases on [`TestStorage`].
#[maybe_async]
#[keep_async_fn]
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
pub trait TestDatabaseBuilder: Sized {
    /// `build_lmdb` at the builder's path, or SlateDB on a fresh in-memory object store.
    async fn build_test_db(self) -> Result<Database<TestStorage>, DatabaseError>;

    /// The async path's stand-in for the sync-only `DataBaseBuilder::build_in_memory`: SlateDB
    /// on a fresh in-memory object store, or LMDB in a temporary directory.
    #[async_only]
    async fn build_in_memory(self) -> Result<Database<TestStorage>, DatabaseError>;
}

#[maybe_async]
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
impl TestDatabaseBuilder for DataBaseBuilder {
    async fn build_test_db(self) -> Result<Database<TestStorage>, DatabaseError> {
        #[cfg(feature = "lmdb")]
        let database = self.build_lmdb();
        #[cfg(all(not(feature = "lmdb"), feature = "slatedb"))]
        let database = {
            let storage = in_memory_slatedb().await?;
            self.build_with_storage(storage)
        };
        database
    }

    #[async_only]
    async fn build_in_memory(self) -> Result<Database<TestStorage>, DatabaseError> {
        #[cfg(feature = "lmdb")]
        let storage = {
            // LMDB keeps its files open, so the directory can go as soon as it is open.
            let dir = tempfile::TempDir::new()?;
            kite_sql::storage::lmdb::LmdbStorage::new(dir.path())?
        };
        #[cfg(all(not(feature = "lmdb"), feature = "slatedb"))]
        let storage = in_memory_slatedb().await?;
        self.build_with_storage(storage)
    }
}
