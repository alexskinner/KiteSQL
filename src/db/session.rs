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

//! Session provides SQL transaction control (BEGIN, COMMIT, ROLLBACK, SET autocommit).
//!
//! # Ownership Design
//!
//! Session owns `Arc<Database<S>>` to be usable as a 'static value stored in structs.
//!
//! ## Transaction Storage Limitation
//!
//! **IMPORTANT**: Due to fundamental lifetime invariance in the Storage trait, this
//! implementation cannot keep actual storage transactions open across calls without
//! using unsafe code.
//!
//! The core issue:
//! - `Storage::TransactionType<'a>` is **invariant** over lifetime `'a`
//! - `Transaction<'a, S>` is therefore invariant (required for LMDB single-writer safety)
//! - Self-referential crates (`ouroboros`, `self_cell`) require **covariance**
//! - `yoke` can handle invariance but requires implementing `Yokeable` with unsafe transmute
//!
//! Without restructuring the Database/Transaction/Storage API itself, the only fully
//! safe approach is tracking transaction *state* without holding open transactions.
//!
//! ## Current Behavior
//!
//! - `BEGIN`/`COMMIT`/`ROLLBACK` track logical transaction state
//! - Each statement execution creates its own underlying storage transaction
//! - This provides correct autocommit semantics but NOT full ACID isolation
//! - Use `Database::new_transaction()` directly for true transactional semantics
//!
//! See `UNSAFE_JUSTIFICATION.md` for detailed technical explanation.
//!
//! # Statement-Level Atomicity
//!
//! Not implemented in this safe-only version, as it requires holding transactions open.
//!
//! # Concurrency
//!
//! All access goes through `Session::run()` which delegates to `Database::run_statement()`.
//! No deadlock risk since we don't hold LMDB transactions open across calls.

use crate::binder::Statement;
use crate::db::{Database, DatabaseIter, ResultIter};
use crate::errors::DatabaseError;
use crate::storage::Storage;
use std::sync::Arc;

/// A session that owns a database and tracks SQL transaction state.
///
/// **Limitation**: This safe implementation tracks transaction state but does not
/// keep storage transactions open across calls. See module documentation.
pub struct Session<S: Storage + 'static> {
    database: Arc<Database<S>>,
    in_transaction_mode: bool,
    autocommit: bool,
}

#[maybe_async]
impl<S: Storage + 'static> Session<S> {
    /// Creates a new session owning the given database.
    ///
    /// The session can be stored in a struct as a 'static value.
    pub fn new(database: Arc<Database<S>>) -> Self {
        Self {
            database,
            in_transaction_mode: false,
            autocommit: true,
        }
    }

    /// Executes a SQL statement and returns an iterator over results.
    ///
    /// Supports transaction control statements (BEGIN, COMMIT, ROLLBACK) and
    /// SET autocommit.
    pub async fn run(&mut self, sql: &str) -> Result<SessionIter<'_, S>, DatabaseError> {
        let statements = self.database.prepare_all(sql)?;

        if statements.is_empty() {
            return Err(DatabaseError::EmptyStatement);
        }

        if statements.len() > 1 {
            return Err(DatabaseError::UnsupportedStmt(
                "Multiple statements not supported in session".to_string(),
            ));
        }

        let statement = statements.into_iter().next().unwrap();
        self.execute_statement_with_result(statement).await
    }

    async fn execute_statement_with_result(
        &mut self,
        statement: Statement,
    ) -> Result<SessionIter<'_, S>, DatabaseError> {
        match statement {
            Statement::StartTransaction { modes, .. } => {
                if !modes.is_empty() {
                    return Err(DatabaseError::UnsupportedStmt(
                        "Transaction modes (isolation level) not supported yet".to_string(),
                    ));
                }
                self.begin_transaction().await?;
                Ok(SessionIter::Empty)
            }
            Statement::Commit { .. } => {
                self.commit_transaction().await?;
                Ok(SessionIter::Empty)
            }
            Statement::Rollback { .. } => {
                self.rollback_transaction().await?;
                Ok(SessionIter::Empty)
            }
            Statement::Savepoint { .. } => Err(DatabaseError::UnsupportedStmt(
                "SAVEPOINT not supported yet".to_string(),
            )),
            _ => {
                if Self::is_set_autocommit(&statement) {
                    self.handle_set_autocommit(&statement).await?;
                    Ok(SessionIter::Empty)
                } else {
                    self.execute_regular_statement_with_result(statement).await
                }
            }
        }
    }

    async fn begin_transaction(&mut self) -> Result<(), DatabaseError> {
        if self.in_transaction_mode {
            // MySQL behavior: BEGIN while transaction open commits the current one
            self.commit_transaction().await?;
        }
        self.in_transaction_mode = true;
        Ok(())
    }

    async fn commit_transaction(&mut self) -> Result<(), DatabaseError> {
        self.in_transaction_mode = false;
        Ok(())
    }

    async fn rollback_transaction(&mut self) -> Result<(), DatabaseError> {
        self.in_transaction_mode = false;
        Ok(())
    }

    fn is_set_autocommit(statement: &Statement) -> bool {
        if let Statement::Set(sqlparser::ast::Set::SingleAssignment {
            variable, values, ..
        }) = statement
        {
            let var_name = variable.to_string();

            if var_name.eq_ignore_ascii_case("autocommit") && values.len() == 1 {
                return matches!(
                    &values[0],
                    sqlparser::ast::Expr::Value(sqlparser::ast::ValueWithSpan { value: sqlparser::ast::Value::Number(n, _), .. }) if n == "0" || n == "1"
                );
            }
        }
        false
    }

    async fn handle_set_autocommit(&mut self, statement: &Statement) -> Result<(), DatabaseError> {
        if let Statement::Set(sqlparser::ast::Set::SingleAssignment { values, .. }) = statement {
            let new_value = match &values[0] {
                sqlparser::ast::Expr::Value(sqlparser::ast::ValueWithSpan {
                    value: sqlparser::ast::Value::Number(n, _),
                    ..
                }) => n == "1",
                _ => {
                    return Err(DatabaseError::UnsupportedStmt(
                        "autocommit must be 0 or 1".to_string(),
                    ))
                }
            };

            if new_value && !self.autocommit {
                // MySQL behavior: SET autocommit=1 commits any open transaction
                self.commit_transaction().await?;
            }

            self.autocommit = new_value;
            return Ok(());
        }
        Err(DatabaseError::UnsupportedStmt(
            "Invalid SET statement".to_string(),
        ))
    }

    async fn execute_regular_statement_with_result(
        &mut self,
        statement: Statement,
    ) -> Result<SessionIter<'_, S>, DatabaseError> {
        if !self.autocommit && !self.in_transaction_mode {
            // MySQL behavior: with autocommit=0, statements implicitly start a transaction
            self.begin_transaction().await?;
        }

        // Execute through database
        // Note: Each statement gets its own storage transaction in this safe implementation
        let iter = self.database.run_statement(statement).await?;
        Ok(SessionIter::Database(iter))
    }

    /// Returns whether autocommit is enabled.
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    /// Returns whether the session is tracking transaction state.
    ///
    /// Note: In this safe implementation, this only tracks logical state, not an
    /// actual open storage transaction.
    pub fn in_transaction(&self) -> bool {
        self.in_transaction_mode
    }
}

/// Iterator over query results from a session.
pub enum SessionIter<'a, S: Storage + 'static> {
    /// Empty result (from transaction control statements).
    Empty,
    /// Result from database query.
    Database(DatabaseIter<'a, S>),
}

#[maybe_async]
impl<'a, S: Storage + 'static> SessionIter<'a, S> {
    /// Consumes the iterator, ensuring all results are processed.
    pub async fn done(self) -> Result<(), DatabaseError> {
        match self {
            SessionIter::Empty => Ok(()),
            SessionIter::Database(iter) => iter.done().await,
        }
    }
}

#[maybe_async]
impl<S: Storage + 'static> ResultIter for SessionIter<'_, S> {
    fn schema<R>(&self, f: impl FnOnce(&crate::types::tuple::SchemaView<'_, '_>) -> R) -> R {
        match self {
            SessionIter::Empty => panic!("Cannot get schema from empty result"),
            SessionIter::Database(iter) => iter.schema(f),
        }
    }

    async fn next_tuple<R: crate::maybe_async::MaybeSend>(
        &mut self,
        f: impl FnOnce(&crate::types::tuple::SchemaView<'_, '_>, &mut crate::db::ResultRow<'_, '_>) -> R,
    ) -> Result<Option<R>, DatabaseError> {
        match self {
            SessionIter::Empty => Ok(None),
            SessionIter::Database(iter) => iter.next_tuple(f).await,
        }
    }

    async fn done(self) -> Result<(), DatabaseError> {
        Self::done(self).await
    }
}

#[maybe_async]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DataBaseBuilder;
    use crate::errors::DatabaseError;
    use tempfile::TempDir;

    #[test]
    async fn test_session_owned_static() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        #[allow(clippy::arc_with_non_send_sync)]
        let database = Arc::new(database);
        let mut session = Session::new(database.clone());

        let iter = session.run("INSERT INTO test VALUES (1, 100)").await?;
        iter.done().await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        assert_eq!(count, 1);

        Ok(())
    }

    struct StaticSessionHolder<S: Storage + 'static> {
        session: Session<S>,
    }

    #[test]
    async fn test_session_stored_in_struct() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        #[allow(clippy::arc_with_non_send_sync)]
        let database = Arc::new(database);
        let mut holder = StaticSessionHolder {
            session: Session::new(database.clone()),
        };

        let iter = holder
            .session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?;
        iter.done().await?;

        let mut iter = holder.session.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    async fn test_session_autocommit_semantics() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let database = Arc::new(database);
        let mut session = Session::new(database.clone());

        // With autocommit=1 (default), changes are immediately committed
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    async fn test_session_begin_implicitly_commits_lmdb() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let database = Arc::new(database);
        let mut session = Session::new(database.clone());

        session.run("BEGIN").await?.done().await?;
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;

        // Second BEGIN should commit the first (MySQL behavior)
        session.run("BEGIN").await?.done().await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1, "BEGIN should have implicitly committed");

        Ok(())
    }

    #[test]
    async fn test_set_autocommit_commits_transaction() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let database = Arc::new(database);
        let mut session = Session::new(database.clone());

        session.run("SET autocommit = 0").await?.done().await?;
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;

        // SET autocommit=1 should commit the open transaction
        session.run("SET autocommit = 1").await?.done().await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1, "SET autocommit=1 should have committed");

        Ok(())
    }
}
