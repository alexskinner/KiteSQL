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
//! # Usage Pattern
//!
//! Session borrows a Database and holds an open transaction across multiple operations.
//! For long-lived driver connections, the recommended pattern is a per-connection worker
//! thread that owns the Database (or an Arc of it) and creates the Session in that scope:
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use std::thread;
//! use std::sync::mpsc::channel;
//! use kite_sql::db::{Database, Session};
//!
//! // Connection worker thread owns Arc<Database> and Session
//! thread::spawn(move || {
//!     let database = Arc::new(database);
//!     let mut session = database.session();
//!     
//!     for command in rx {
//!         session.run(&command).unwrap();
//!     }
//! });
//! ```
//!
//! # Concurrency
//!
//! **Important**: When a session transaction is open, all database access must go through
//! that session. Attempting to use `Database::run()` on the same thread while a session
//! holds an LMDB write transaction will deadlock (LMDB is single-writer). Use `Session::run()`
//! for all operations while the session is active.
//!
//! # Statement-Level Atomicity Limitation
//!
//! **Known limitation**: A statement that fails mid-execution inside an explicit transaction
//! may leave partial writes until ROLLBACK. MySQL semantics require that only the failing
//! statement's effects are rolled back while keeping the transaction open, but this is not
//! currently implemented.
//!
//! Implementation will be added in a follow-up PR using one of:
//! - Capturing inverse operations in DML executors before writes
//! - LMDB nested transactions (RwTransaction::begin_nested_txn)
//! - Write buffering at the executor level

use crate::binder::Statement;
use crate::db::{
    BindSource, Database, DatabaseIter, Executor, ResultIter, Transaction, TransactionIter,
};
use crate::errors::DatabaseError;
use crate::planner::PlanArena;
use crate::storage::Storage;

/// A session that borrows a database and manages SQL transaction state.
pub struct Session<'db, S: Storage> {
    database: &'db Database<S>,
    active_transaction: Option<Transaction<'db, S>>,
    autocommit: bool,
}

#[maybe_async]
impl<'db, S: Storage> Session<'db, S> {
    /// Creates a new session borrowing the given database.
    pub fn new(database: &'db Database<S>) -> Self {
        Self {
            database,
            active_transaction: None,
            autocommit: true,
        }
    }

    /// Executes a SQL statement and returns an iterator over results.
    ///
    /// Supports transaction control statements (BEGIN, COMMIT, ROLLBACK) and
    /// SET autocommit. Other statements execute within the session's transaction
    /// context (or in autocommit mode).
    pub async fn run<'a>(
        &'a mut self,
        sql: &str,
    ) -> Result<SessionIter<'a, 'db, S>, DatabaseError> {
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

    async fn execute_statement_with_result<'a>(
        &'a mut self,
        statement: Statement,
    ) -> Result<SessionIter<'a, 'db, S>, DatabaseError> {
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
        // MySQL behavior: BEGIN while transaction open commits it
        if let Some(txn) = self.active_transaction.take() {
            txn.commit().await?;
        }

        let txn = self.database.new_transaction(false)?;
        self.active_transaction = Some(txn);

        Ok(())
    }

    async fn commit_transaction(&mut self) -> Result<(), DatabaseError> {
        if let Some(txn) = self.active_transaction.take() {
            txn.commit().await?;
        }

        Ok(())
    }

    async fn rollback_transaction(&mut self) -> Result<(), DatabaseError> {
        if let Some(_txn) = self.active_transaction.take() {
            // Transaction dropped, automatically rolls back
        }

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

            // MySQL behavior: SET autocommit=1 commits any open transaction
            if new_value && !self.autocommit {
                if let Some(txn) = self.active_transaction.take() {
                    txn.commit().await?;
                }
            }

            self.autocommit = new_value;
            return Ok(());
        }
        Err(DatabaseError::UnsupportedStmt(
            "Invalid SET statement".to_string(),
        ))
    }

    async fn execute_regular_statement_with_result<'a>(
        &'a mut self,
        statement: Statement,
    ) -> Result<SessionIter<'a, 'db, S>, DatabaseError> {
        // MySQL behavior: with autocommit=0, implicitly start transaction
        if !self.autocommit && self.active_transaction.is_none() {
            self.begin_transaction().await?;
        }

        // Execute within the session's transaction
        if let Some(ref mut txn) = self.active_transaction {
            // For statement-level atomicity: execute and handle errors
            // If execution fails, the transaction remains open (MySQL semantics)

            let result = BindSource::execute(txn, false, |state, tx| {
                state
                    .build_plan(&[], tx, |binder, arena| binder.bind(&statement, arena))
                    .map(Executor::from_plan)
            })
            .await?;

            Ok(SessionIter::Transaction(Box::new(result)))
        } else {
            // Autocommit mode: use database.run_statement which handles transaction lifecycle
            // Statement atomicity is inherent here - if it fails, transaction is dropped
            let iter = self.database.run_statement(statement).await?;
            Ok(SessionIter::Database(iter))
        }
    }

    /// Returns whether autocommit is enabled.
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    /// Returns whether a transaction is currently open.
    pub fn in_transaction(&self) -> bool {
        self.active_transaction.is_some()
    }
}

/// Iterator over query results from a session.
pub enum SessionIter<'a, 'db, S: Storage + 'db> {
    /// Empty result (from transaction control statements).
    Empty,
    /// Result from transaction query.
    Transaction(Box<TransactionIter<'a, 'a, S::TransactionType<'db>, PlanArena<'a>>>),
    /// Result from database query (autocommit mode).
    Database(DatabaseIter<'db, S>),
}

#[maybe_async]
impl<'a, 'db, S: Storage + 'db> SessionIter<'a, 'db, S> {
    /// Consumes the iterator, ensuring all results are processed.
    pub async fn done(self) -> Result<(), DatabaseError> {
        match self {
            SessionIter::Empty => Ok(()),
            SessionIter::Transaction(iter) => (*iter).done().await,
            SessionIter::Database(iter) => iter.done().await,
        }
    }
}

#[maybe_async]
impl<'a, 'db, S: Storage + 'db> ResultIter for SessionIter<'a, 'db, S> {
    fn schema<R>(&self, f: impl FnOnce(&crate::types::tuple::SchemaView<'_, '_>) -> R) -> R {
        match self {
            SessionIter::Empty => panic!("Cannot get schema from empty result"),
            SessionIter::Transaction(iter) => iter.schema(f),
            SessionIter::Database(iter) => iter.schema(f),
        }
    }

    async fn next_tuple<R: crate::maybe_async::MaybeSend>(
        &mut self,
        f: impl FnOnce(&crate::types::tuple::SchemaView<'_, '_>, &mut crate::db::ResultRow<'_, '_>) -> R
            + crate::maybe_async::MaybeSend,
    ) -> Result<Option<R>, DatabaseError> {
        match self {
            SessionIter::Empty => Ok(None),
            SessionIter::Transaction(iter) => iter.next_tuple(f).await,
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
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    async fn test_session_basic_transaction() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = database.session();

        session.run("BEGIN").await?.done().await?;
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;
        session.run("COMMIT").await?.done().await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    async fn test_session_rollback() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = database.session();

        session.run("BEGIN").await?.done().await?;
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;
        session.run("ROLLBACK").await?.done().await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 0, "ROLLBACK should discard changes");

        Ok(())
    }

    #[test]
    async fn test_session_begin_implicitly_commits_lmdb() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = database.session();

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
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1, "BEGIN should have implicitly committed");

        Ok(())
    }

    #[test]
    async fn test_session_all_access_through_session() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = database.session();

        session.run("BEGIN").await?.done().await?;

        // All operations through session while transaction is open
        session
            .run("INSERT INTO test VALUES (1, 100)")
            .await?
            .done()
            .await?;

        let mut iter = session.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 1);

        session
            .run("INSERT INTO test VALUES (2, 200)")
            .await?
            .done()
            .await?;

        let mut iter = session.run("SELECT * FROM test ORDER BY id").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 2);

        session.run("COMMIT").await?.done().await?;

        Ok(())
    }

    #[test]
    fn test_per_connection_thread_pattern() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database.ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")?;

        let database = Arc::new(database);
        let (tx, rx) = channel::<String>();

        // Connection worker thread owns Arc<Database> and Session
        let db_clone = database.clone();
        let handle = thread::spawn(move || {
            let mut session = db_clone.session();

            for command in rx {
                if command == "DONE" {
                    break;
                }
                session.run(&command).unwrap().done().unwrap();
            }
        });

        // Send commands to the worker
        tx.send("BEGIN".to_string()).unwrap();
        tx.send("INSERT INTO test VALUES (1, 100)".to_string())
            .unwrap();
        tx.send("COMMIT".to_string()).unwrap();
        tx.send("DONE".to_string()).unwrap();

        handle.join().unwrap();

        // Verify results
        let mut iter = database.run("SELECT * FROM test")?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        assert_eq!(count, 1);

        Ok(())
    }
}
