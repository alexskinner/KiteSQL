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
//! # Statement-Level Atomicity
//!
//! When DML statements run inside an explicit transaction (BEGIN/COMMIT), KiteSQL provides
//! statement-level atomicity matching MySQL: if a statement fails partway through, only that
//! statement's changes are rolled back while the transaction remains open.
//!
//! Implementation: DML executors record inverse operations after each successful write. If
//! the statement errors during iteration or done(), the inverses replay in reverse, undoing
//! only that statement. On success, the undo log is discarded. Inside an explicit transaction,
//! the undo log holds per-row state until the statement ends, so very large single statements
//! use proportional memory.
//!
//! In autocommit mode, statement undo is skipped for performance since the entire transaction
//! is dropped on failure anyway.
//!
//! Note: Memory storage does not support rollback, so statement atomicity is unavailable there.

use crate::binder::Statement;
use crate::db::{
    Database, DatabaseIter, Executor, PrivateCatalog, ResultIter, Transaction, TransactionIter,
};
use crate::errors::DatabaseError;
use crate::planner::PlanArena;
use crate::storage::Storage;

/// A session that borrows a database and manages SQL transaction state.
pub struct Session<'db, S: Storage> {
    database: &'db Database<S>,
    active_transaction: Option<Transaction<'db, S>>,
    autocommit: bool,
    /// Catalog this transaction binds against once it has run DDL or ANALYZE: built from the
    /// transaction's own view of storage, never shared with other sessions or the database.
    private_catalog: Option<PrivateCatalog>,
    /// The next statement must rebuild `private_catalog` from the transaction first.
    catalog_dirty: bool,
    /// The transaction ran DDL/ANALYZE: publish the committed catalog when it commits.
    catalog_changed: bool,
}

#[maybe_async]
impl<'db, S: Storage> Session<'db, S> {
    /// Creates a new session borrowing the given database.
    pub fn new(database: &'db Database<S>) -> Self {
        Self {
            database,
            active_transaction: None,
            autocommit: true,
            private_catalog: None,
            catalog_dirty: false,
            catalog_changed: false,
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
        self.finish_transaction(true).await?;

        // Enable statement-level undo for explicit transactions
        let txn = self.database.new_transaction_with_undo(false, true)?;
        self.active_transaction = Some(txn);

        Ok(())
    }

    async fn commit_transaction(&mut self) -> Result<(), DatabaseError> {
        self.finish_transaction(true).await
    }

    async fn rollback_transaction(&mut self) -> Result<(), DatabaseError> {
        // Transaction dropped, automatically rolls back; its private catalog goes with it.
        self.finish_transaction(false).await
    }

    /// Ends the open transaction. A commit that ran DDL publishes the new catalog to the
    /// database; a rollback (or a failed commit) discards the private catalog and publishes
    /// nothing.
    async fn finish_transaction(&mut self, commit: bool) -> Result<(), DatabaseError> {
        let changed = std::mem::take(&mut self.catalog_changed);
        self.private_catalog = None;
        self.catalog_dirty = false;
        let Some(txn) = self.active_transaction.take() else {
            return Ok(());
        };
        if commit {
            txn.commit().await?;
            if changed {
                self.database.publish_catalog().await?;
            }
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
                self.finish_transaction(true).await?;
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
        if let Some(txn) = self.active_transaction.as_mut() {
            // After DDL the shared catalog is stale for this transaction: rebuild the private
            // one from what the transaction itself sees (its uncommitted DDL included).
            if self.catalog_dirty {
                let state = &self.database.state;
                let base = match &self.private_catalog {
                    Some(private) => private.as_ref(),
                    None => txn.catalog.view(),
                };
                let rebuilt = state.build_catalog(base, &txn.inner).await?;
                self.private_catalog = Some(rebuilt);
                self.catalog_dirty = false;
            }

            // Bind against the generation this transaction pinned when it began (or its private
            // catalog after its own DDL), never the latest published one.
            let state = &self.database.state;
            let catalog = match &self.private_catalog {
                Some(private) => private.as_ref(),
                None => txn.catalog.view(),
            };

            // For statement-level atomicity: execute and handle errors
            // If execution fails, the transaction remains open (MySQL semantics)
            let (plan, arena) =
                state.build_plan_in(catalog, &[], &txn.inner, |binder, arena| {
                    binder.bind(&statement, arena)
                })?;
            if matches!(
                plan.operator.command_type(),
                crate::planner::operator::CommandType::DDL
                    | crate::planner::operator::CommandType::Analyze
            ) {
                // Even a failed DDL may have written part of its change: rebuild either way.
                self.catalog_dirty = true;
                self.catalog_changed = true;
            }
            let executor = Executor::new(plan, &arena);
            let (arena, executor) = state
                .execute(&mut txn.inner, executor, arena, txn.enable_undo)
                .await?;
            let result = TransactionIter::new(arena, executor);

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
#[cfg(all(test, feature = "lmdb"))]
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

    async fn null_unique_rows<S: Storage>(
        database: &Database<S>,
        sql: &str,
    ) -> Result<Vec<String>, DatabaseError> {
        let mut iter = database.run(sql).await?;
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                tuple
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .await?
        {
            rows.push(row);
        }
        iter.done().await?;
        Ok(rows)
    }

    async fn null_unique_exec<S: Storage>(
        database: &Database<S>,
        sql: &str,
    ) -> Result<(), DatabaseError> {
        database.run(sql).await?.done().await
    }

    async fn odku_rows<S: Storage>(
        database: &Database<S>,
        sql: &str,
    ) -> Result<Vec<String>, DatabaseError> {
        let mut iter = database.run(sql).await?;
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                tuple
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .await?
        {
            rows.push(row);
        }
        iter.done().await?;
        Ok(rows)
    }

    async fn odku_database(
    ) -> Result<(TempDir, Database<crate::storage::lmdb::LmdbStorage>), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT, c INT)")
            .await?;
        database
            .run("INSERT INTO t VALUES (1, 10, 0)")
            .await?
            .done()
            .await?;
        Ok((temp_dir, database))
    }

    #[test]
    async fn test_unique_index_allows_many_nulls_on_insert() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(50) UNIQUE)")
            .await?;
        null_unique_exec(&database, "INSERT INTO u VALUES (1, NULL), (2, NULL)").await?;
        null_unique_exec(&database, "INSERT INTO u VALUES (3, NULL), (4, 'a')").await?;
        assert_eq!(
            null_unique_rows(
                &database,
                "SELECT id FROM u WHERE email IS NULL ORDER BY id"
            )
            .await?,
            vec!["1", "2", "3"]
        );
        assert!(matches!(
            null_unique_exec(&database, "INSERT INTO u VALUES (5, 'a')").await,
            Err(DatabaseError::DuplicateUniqueValue)
        ));
        null_unique_exec(&database, "DELETE FROM u WHERE id = 2").await?;
        assert_eq!(
            null_unique_rows(
                &database,
                "SELECT id FROM u WHERE email IS NULL ORDER BY id"
            )
            .await?,
            vec!["1", "3"]
        );
        Ok(())
    }

    #[test]
    async fn test_unique_index_update_into_null() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(50) UNIQUE)")
            .await?;
        null_unique_exec(
            &database,
            "INSERT INTO u VALUES (1, 'a'), (2, 'b'), (3, NULL)",
        )
        .await?;
        null_unique_exec(&database, "UPDATE u SET email = NULL WHERE id < 3").await?;
        assert_eq!(
            null_unique_rows(
                &database,
                "SELECT id FROM u WHERE email IS NULL ORDER BY id"
            )
            .await?,
            vec!["1", "2", "3"]
        );
        // The old values were released, and NULL rows can take values again.
        null_unique_exec(&database, "INSERT INTO u VALUES (4, 'a')").await?;
        null_unique_exec(&database, "UPDATE u SET email = 'b' WHERE id = 2").await?;
        assert!(matches!(
            null_unique_exec(&database, "UPDATE u SET email = 'b' WHERE id = 1").await,
            Err(DatabaseError::DuplicateUniqueValue)
        ));
        assert_eq!(
            null_unique_rows(&database, "SELECT * FROM u ORDER BY id").await?,
            vec!["1|NULL", "2|b", "3|NULL", "4|a"]
        );
        Ok(())
    }

    #[test]
    async fn test_composite_unique_index_partial_null() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE cu (id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY k (a, b))")
            .await?;
        null_unique_exec(
            &database,
            "INSERT INTO cu VALUES (1, 1, NULL), (2, 1, NULL), (3, NULL, NULL), (4, 1, 1)",
        )
        .await?;
        null_unique_exec(&database, "INSERT INTO cu VALUES (5, NULL, NULL)").await?;
        assert!(matches!(
            null_unique_exec(&database, "INSERT INTO cu VALUES (6, 1, 1)").await,
            Err(DatabaseError::DuplicateUniqueValue)
        ));
        assert_eq!(
            null_unique_rows(&database, "SELECT id FROM cu WHERE a = 1 ORDER BY id").await?,
            vec!["1", "2", "4"]
        );
        assert_eq!(
            null_unique_rows(&database, "SELECT id FROM cu WHERE a = 1 AND b = 1").await?,
            vec!["4"]
        );
        null_unique_exec(&database, "UPDATE cu SET b = NULL WHERE id = 4").await?;
        assert_eq!(
            null_unique_rows(&database, "SELECT id FROM cu WHERE a = 1 ORDER BY id").await?,
            vec!["1", "2", "4"]
        );
        assert!(
            null_unique_rows(&database, "SELECT id FROM cu WHERE a = 1 AND b = 1")
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    async fn test_unique_index_nulls_survive_reopen() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        {
            let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
            database
                .ddl("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(50) UNIQUE)")
                .await?;
            null_unique_exec(
                &database,
                "INSERT INTO u VALUES (1, NULL), (2, NULL), (3, 'a')",
            )
            .await?;
        }
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .load(crate::db::CatalogKind::Table("u".to_string()))
            .await?;
        assert_eq!(
            null_unique_rows(
                &database,
                "SELECT id FROM u WHERE email IS NULL ORDER BY id"
            )
            .await?,
            vec!["1", "2"]
        );
        null_unique_exec(&database, "INSERT INTO u VALUES (4, NULL)").await?;
        null_unique_exec(&database, "DELETE FROM u WHERE id = 1").await?;
        assert_eq!(
            null_unique_rows(
                &database,
                "SELECT id FROM u WHERE email IS NULL ORDER BY id"
            )
            .await?,
            vec!["2", "4"]
        );
        assert!(matches!(
            null_unique_exec(&database, "INSERT INTO u VALUES (5, 'a')").await,
            Err(DatabaseError::DuplicateUniqueValue)
        ));
        Ok(())
    }
    #[test]
    async fn test_odku_set_reads_existing_row() -> Result<(), DatabaseError> {
        let (_dir, database) = odku_database().await?;
        let sql =
            "INSERT INTO t VALUES (1, 3, 0) ON DUPLICATE KEY UPDATE v = v + VALUES(v), c = c + 1";
        assert_eq!(odku_rows(&database, sql).await?, vec!["2"]);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t").await?,
            vec!["1|13|1"]
        );

        // The second row conflicts with the first row of the same statement.
        let sql =
            "INSERT INTO t VALUES (3, 1, 0), (3, 5, 0) ON DUPLICATE KEY UPDATE v = v + VALUES(v)";
        assert_eq!(odku_rows(&database, sql).await?, vec!["3"]);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t WHERE id = 3").await?,
            vec!["3|6|0"]
        );
        Ok(())
    }

    #[test]
    async fn test_odku_primary_key_change_moves_row() -> Result<(), DatabaseError> {
        let (_dir, database) = odku_database().await?;
        database
            .run("INSERT INTO t VALUES (2, 20, 0)")
            .await?
            .done()
            .await?;

        // Inside a transaction the move is undone by ROLLBACK.
        let mut session = database.session();
        session.run("BEGIN").await?.done().await?;
        session
            .run("INSERT INTO t VALUES (1, 0, 0) ON DUPLICATE KEY UPDATE id = 50")
            .await?
            .done()
            .await?;
        session.run("ROLLBACK").await?.done().await?;
        drop(session);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t ORDER BY id").await?,
            vec!["1|10|0", "2|20|0"]
        );

        let sql = "INSERT INTO t VALUES (1, 0, 0) ON DUPLICATE KEY UPDATE id = 50";
        assert_eq!(odku_rows(&database, sql).await?, vec!["2"]);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t ORDER BY id").await?,
            vec!["2|20|0", "50|10|0"]
        );
        assert!(odku_rows(&database, "SELECT * FROM t WHERE id = 1")
            .await?
            .is_empty());
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t WHERE id = 50").await?,
            vec!["50|10|0"]
        );

        // Moving onto a key that exists is a duplicate primary key.
        let sql = "INSERT INTO t VALUES (2, 0, 0) ON DUPLICATE KEY UPDATE id = 50";
        assert!(matches!(
            odku_rows(&database, sql).await,
            Err(DatabaseError::DuplicatePrimaryKey)
        ));
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t ORDER BY id").await?,
            vec!["2|20|0", "50|10|0"]
        );
        Ok(())
    }

    #[test]
    async fn test_odku_affected_rows_without_undo() -> Result<(), DatabaseError> {
        let (_dir, database) = odku_database().await?;
        let changed = "INSERT INTO t VALUES (1, 20, 0) ON DUPLICATE KEY UPDATE v = VALUES(v)";
        assert_eq!(odku_rows(&database, changed).await?, vec!["2"]);
        assert_eq!(odku_rows(&database, changed).await?, vec!["0"]);
        let inserted = "INSERT INTO t VALUES (2, 5, 0) ON DUPLICATE KEY UPDATE v = VALUES(v)";
        assert_eq!(odku_rows(&database, inserted).await?, vec!["1"]);
        Ok(())
    }

    #[test]
    async fn test_odku_null_unique_key_is_not_a_conflict() -> Result<(), DatabaseError> {
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE u (id INT PRIMARY KEY, email VARCHAR(50) UNIQUE, n INT)")
            .await?;
        database
            .ddl("CREATE TABLE cu (id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY k (a, b))")
            .await?;
        database
            .run("INSERT INTO u VALUES (1, 'a', 0)")
            .await?
            .done()
            .await?;
        database
            .run("INSERT INTO cu VALUES (1, 1, 1)")
            .await?
            .done()
            .await?;

        // A NULL key is inserted, not matched against anything.
        let sql = "INSERT INTO u VALUES (2, NULL, 5) ON DUPLICATE KEY UPDATE n = n + 1";
        assert_eq!(odku_rows(&database, sql).await?, vec!["1"]);
        let sql = "INSERT INTO cu VALUES (2, 1, NULL) ON DUPLICATE KEY UPDATE id = 99";
        assert_eq!(odku_rows(&database, sql).await?, vec!["1"]);

        // A second NULL key must never update the row holding the first one. Whether the
        // storage layer then accepts the second NULL is a separate, core matter.
        let sql = "INSERT INTO u VALUES (3, NULL, 7) ON DUPLICATE KEY UPDATE n = n + 1";
        let updated = matches!(odku_rows(&database, sql).await.as_deref(), Ok([r]) if r == "2");
        assert!(!updated);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM u WHERE id = 2").await?,
            vec!["2|NULL|5"]
        );
        let sql = "INSERT INTO cu VALUES (3, 1, NULL) ON DUPLICATE KEY UPDATE id = 99";
        let updated = matches!(odku_rows(&database, sql).await.as_deref(), Ok([r]) if r == "2");
        assert!(!updated);
        assert!(odku_rows(&database, "SELECT * FROM cu WHERE id = 99")
            .await?
            .is_empty());
        assert_eq!(
            odku_rows(&database, "SELECT * FROM cu WHERE id = 2").await?,
            vec!["2|1|NULL"]
        );

        // Non-NULL keys still conflict.
        let sql = "INSERT INTO u VALUES (4, 'a', 0) ON DUPLICATE KEY UPDATE n = 9";
        assert_eq!(odku_rows(&database, sql).await?, vec!["2"]);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM u WHERE email = 'a'").await?,
            vec!["1|a|9"]
        );
        Ok(())
    }

    #[test]
    async fn test_odku_row_alias() -> Result<(), DatabaseError> {
        let (_dir, database) = odku_database().await?;
        let sql = "INSERT INTO t VALUES (1, 7, 0) AS new ON DUPLICATE KEY UPDATE v = new.v";
        assert_eq!(odku_rows(&database, sql).await?, vec!["2"]);
        let sql = "INSERT INTO t VALUES (1, 8, 0) AS New ON DUPLICATE KEY UPDATE v = NEW.v + v";
        assert_eq!(odku_rows(&database, sql).await?, vec!["2"]);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t").await?,
            vec!["1|15|0"]
        );

        let sql = "INSERT INTO t VALUES (1, 1, 1) AS n (a, b, c) ON DUPLICATE KEY UPDATE v = n.b";
        assert!(matches!(
            odku_rows(&database, sql).await,
            Err(DatabaseError::UnsupportedStmt(_))
        ));
        Ok(())
    }

    #[test]
    async fn test_odku_statement_is_atomic() -> Result<(), DatabaseError> {
        let (_dir, database) = odku_database().await?;
        database
            .run("INSERT INTO t VALUES (3, 30, 0)")
            .await?
            .done()
            .await?;
        // Row 1 inserts 60; row 2 then fails to move 1 onto the existing key 3.
        let sql = "INSERT INTO t VALUES (60, 0, 0), (1, 0, 0) ON DUPLICATE KEY UPDATE id = 3";
        assert!(odku_rows(&database, sql).await.is_err());
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t ORDER BY id").await?,
            vec!["1|10|0", "3|30|0"]
        );

        let mut session = database.session();
        session.run("BEGIN").await?.done().await?;
        session
            .run("INSERT INTO t VALUES (4, 0, 0)")
            .await?
            .done()
            .await?;
        let result = match session.run(sql).await {
            Ok(iter) => iter.done().await,
            Err(err) => Err(err),
        };
        assert!(result.is_err());
        session.run("COMMIT").await?.done().await?;
        drop(session);
        assert_eq!(
            odku_rows(&database, "SELECT * FROM t ORDER BY id").await?,
            vec!["1|10|0", "3|30|0", "4|0|0"]
        );
        Ok(())
    }
}
