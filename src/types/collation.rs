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

//! Text collation: how text values compare.
//!
//! By default text compares byte-wise. A dialect may supply a [`TextCollation`] (see
//! `SqlDialect::text_collation`): a comparison key per string, so that strings with equal keys
//! are equal and strings order by their keys. While a statement of that dialect runs, every
//! text comparison uses the key: `=`, `<`, `IN`, sorting, grouping, `DISTINCT`, hash joins,
//! `MIN`/`MAX`, and the primary key and index keys of tables created under the collation (so a
//! unique key rejects `'SMITH'` next to `'Smith'` and a lookup for `'smith'` finds both).
//!
//! Like statement diagnostics, the active collation is per thread: preparing a statement
//! installs the dialect's collation, and the statement is bound, planned and iterated on that
//! thread. Index keys hold the key, not the text, so a table created under a collation never
//! answers a scan from its index alone for text columns, and is marked with the collation's
//! name; a table without that mark (created by-bytes) keeps byte-wise keys.

use crate::catalog::TableId;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// The comparison key of a string under a collation.
pub type TextKeyFn = for<'a> fn(&'a str) -> Cow<'a, str>;

/// A named text collation: two strings compare as their keys do.
///
/// The key must be stable: tables created under the collation store it in their keys, so
/// changing what a name maps to needs those tables recreated.
#[derive(Clone, Copy)]
pub struct TextCollation {
    name: &'static str,
    key: TextKeyFn,
}

impl TextCollation {
    pub const fn new(name: &'static str, key: TextKeyFn) -> Self {
        Self { name, key }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn key<'a>(&self, text: &'a str) -> Cow<'a, str> {
        (self.key)(text)
    }
}

impl PartialEq for TextCollation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for TextCollation {}

impl fmt::Debug for TextCollation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TextCollation").field(&self.name).finish()
    }
}

/// The collation state of the statement running on this thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollationState {
    collation: Option<TextCollation>,
    /// Tables of this statement whose keys are byte-wise (created without the collation).
    raw_key_tables: Vec<TableId>,
}

thread_local! {
    static STATE: RefCell<CollationState> = RefCell::new(CollationState::default());
    /// `STATE.collation`, read on every text comparison.
    static ACTIVE: Cell<Option<TextCollation>> = const { Cell::new(None) };
    /// Set while a key of a byte-wise table is encoded.
    static RAW: Cell<bool> = const { Cell::new(false) };
}

/// Starts a statement on this thread under `collation` (`None`: byte-wise).
pub fn begin_statement(collation: Option<TextCollation>) {
    ACTIVE.with(|a| a.set(collation));
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.collation = collation;
        s.raw_key_tables.clear();
    });
}

/// The collation of the statement running on this thread.
#[inline]
pub fn active() -> Option<TextCollation> {
    ACTIVE.with(|a| a.get())
}

/// Snapshot of this thread's state, to run the same statement later (see [`restore`]).
pub fn snapshot() -> CollationState {
    STATE.with(|s| s.borrow().clone())
}

/// Installs a [`snapshot`].
pub fn restore(state: CollationState) {
    ACTIVE.with(|a| a.set(state.collation));
    STATE.with(|s| *s.borrow_mut() = state);
}

/// Marks `table_id`'s keys as byte-wise for the running statement.
pub(crate) fn mark_raw_keys(table_id: TableId) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if !s.raw_key_tables.contains(&table_id) {
            s.raw_key_tables.push(table_id);
        }
    });
}

/// Whether text in `table_id`'s keys is stored as the collation key.
pub(crate) fn folds_keys(table_id: TableId) -> bool {
    STATE.with(|s| {
        let s = s.borrow();
        s.collation.is_some() && !s.raw_key_tables.contains(&table_id)
    })
}

/// Runs `f` with byte-wise text keys (encoding a key of a byte-wise table).
pub(crate) fn with_raw_keys<R>(raw: bool, f: impl FnOnce() -> R) -> R {
    if !raw {
        return f();
    }
    let saved = RAW.with(|r| r.replace(true));
    let result = f();
    RAW.with(|r| r.set(saved));
    result
}

/// The key `text` compares by on this thread.
#[inline]
pub fn text_key(text: &str) -> Cow<'_, str> {
    match active() {
        Some(collation) => collation.key(text),
        None => Cow::Borrowed(text),
    }
}

/// The bytes a memcomparable key holds for `text`: its comparison key, except while the key
/// of a byte-wise table is encoded.
#[inline]
pub(crate) fn encoding_key(text: &str) -> Cow<'_, str> {
    if RAW.with(|r| r.get()) {
        Cow::Borrowed(text)
    } else {
        text_key(text)
    }
}

#[inline]
pub fn text_cmp(left: &str, right: &str) -> Ordering {
    match active() {
        Some(collation) => collation.key(left).cmp(&collation.key(right)),
        None => left.cmp(right),
    }
}

#[inline]
pub fn text_eq(left: &str, right: &str) -> bool {
    match active() {
        Some(collation) => left == right || collation.key(left) == collation.key(right),
        None => left == right,
    }
}

#[inline]
pub fn text_hash<H: Hasher>(text: &str, state: &mut H) {
    text_key(text).hash(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(text: &str) -> Cow<'_, str> {
        if text.chars().any(char::is_uppercase) {
            Cow::Owned(text.to_lowercase())
        } else {
            Cow::Borrowed(text)
        }
    }

    #[test]
    fn active_collation_compares_by_key() {
        begin_statement(None);
        assert!(!text_eq("a", "A"));
        assert_eq!(text_cmp("B", "a"), Ordering::Less);

        begin_statement(Some(TextCollation::new("lower", lower)));
        assert!(text_eq("a", "A"));
        assert_eq!(text_cmp("B", "a"), Ordering::Greater);
        assert_eq!(text_key("ABC"), "abc");
        assert!(folds_keys(7));
        mark_raw_keys(7);
        assert!(!folds_keys(7));
        assert_eq!(
            with_raw_keys(true, || encoding_key("ABC").into_owned()),
            "ABC"
        );
        assert_eq!(encoding_key("ABC"), "abc");
        assert_eq!(text_key("ABC"), "abc");

        let saved = snapshot();
        begin_statement(None);
        assert!(!text_eq("a", "A"));
        restore(saved);
        assert!(text_eq("a", "A"));
        assert!(!folds_keys(7));
        begin_statement(None);
    }
}
