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

//! Statement-scoped dialect state for evaluator access.
//!
//! Like collation, the active dialect is per thread: preparing a statement installs the
//! dialect, and the statement is bound, planned and iterated on that thread.

use super::SqlDialect;
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    static ACTIVE: RefCell<Option<Arc<dyn SqlDialect>>> = RefCell::new(None);
}

/// Starts a statement on this thread under `dialect`.
pub fn begin_statement(dialect: Arc<dyn SqlDialect>) {
    ACTIVE.with(|d| *d.borrow_mut() = Some(dialect));
}

/// The dialect of the statement running on this thread.
#[inline]
pub fn active() -> Option<Arc<dyn SqlDialect>> {
    ACTIVE.with(|d| d.borrow().clone())
}

/// Clears the active dialect (for cleanup or testing).
pub fn end_statement() {
    ACTIVE.with(|d| *d.borrow_mut() = None);
}
