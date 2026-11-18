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

use crate::binder::Statement;
use crate::db::{BindSource, Database, Transaction};
use crate::errors::DatabaseError;
use crate::storage::Storage;

pub struct Session<'db, S: Storage> {
    database: &'db Database<S>,
    active_transaction: Option<Transaction<'db, S>>,
    autocommit: bool,
}

#[maybe_async]
impl<'db, S: Storage> Session<'db, S> {
    pub fn new(database: &'db Database<S>) -> Self {
        Self {
            database,
            active_transaction: None,
            autocommit: true,
        }
    }

    pub async fn execute(&mut self, sql: &str) -> Result<(), DatabaseError> {
        let statements = self.database.prepare_all(sql)?;

        if statements.is_empty() {
            return Ok(());
        }

        if statements.len() > 1 {
            return Err(DatabaseError::UnsupportedStmt(
                "Multiple statements not supported in session".to_string(),
            ));
        }

        let statement = statements.into_iter().next().unwrap();
        self.execute_statement(statement).await
    }

    pub async fn run(&mut self, sql: &str) -> Result<(), DatabaseError> {
        self.execute(sql).await
    }

    async fn execute_statement(&mut self, statement: Statement) -> Result<(), DatabaseError> {
        match statement {
            Statement::StartTransaction { modes, .. } => {
                if !modes.is_empty() {
                    return Err(DatabaseError::UnsupportedStmt(
                        "Transaction modes (isolation level) not supported yet".to_string(),
                    ));
                }
                self.begin_transaction().await
            }
            Statement::Commit { .. } => self.commit_transaction().await,
            Statement::Rollback { .. } => self.rollback_transaction().await,
            Statement::Savepoint { .. } => Err(DatabaseError::UnsupportedStmt(
                "SAVEPOINT not supported yet".to_string(),
            )),
            _ => {
                if Self::is_set_autocommit(&statement) {
                    self.handle_set_autocommit(&statement).await
                } else {
                    self.execute_regular_statement(statement).await
                }
            }
        }
    }

    async fn begin_transaction(&mut self) -> Result<(), DatabaseError> {
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
            drop(_txn);
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

    async fn execute_regular_statement(
        &mut self,
        statement: Statement,
    ) -> Result<(), DatabaseError> {
        if let Some(txn) = &mut self.active_transaction {
            Self::execute_in_transaction_and_drain(txn, &statement).await
        } else if self.autocommit {
            let mut txn = self.database.new_transaction(false)?;
            Self::execute_in_transaction_and_drain(&mut txn, &statement).await?;
            txn.commit().await?;
            Ok(())
        } else {
            let txn = self.database.new_transaction(false)?;
            self.active_transaction = Some(txn);
            self.execute_regular_statement(statement).await
        }
    }

    async fn execute_in_transaction_and_drain(
        txn: &mut Transaction<'_, S>,
        statement: &Statement,
    ) -> Result<(), DatabaseError> {
        use crate::binder::Binder;
        use crate::execution::Executor;

        let iter = txn.execute(false, |state, transaction| {
            let (plan, arena) = state.build_plan(
                &[],
                transaction,
                |binder: &mut Binder<'_, '_, _, _>, arena| binder.bind(statement, arena),
            )?;
            let executor = Executor::new(plan, &arena);
            Ok((executor, arena))
        })?;

        iter.done().await
    }

    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    pub fn in_transaction(&self) -> bool {
        self.active_transaction.is_some()
    }
}

#[maybe_async]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DataBaseBuilder;
    use crate::errors::DatabaseError;

    #[test]
    async fn test_session_begin_commit() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);

        assert!(!session.in_transaction());
        assert!(session.autocommit());

        session.execute("BEGIN").await?;
        assert!(session.in_transaction());

        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        assert!(session.in_transaction());

        session.execute("COMMIT").await?;
        assert!(!session.in_transaction());

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
    async fn test_session_begin_rollback() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("BEGIN").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        session.execute("ROLLBACK").await?;

        assert!(!session.in_transaction());

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 0);

        Ok(())
    }

    #[test]
    async fn test_session_error_in_transaction_keeps_it_open() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("BEGIN").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;

        let result = session.execute("INSERT INTO test VALUES (1, 200)").await;
        assert!(result.is_err());

        assert!(session.in_transaction());

        session.execute("INSERT INTO test VALUES (2, 200)").await?;
        session.execute("COMMIT").await?;

        let mut iter = database.run("SELECT * FROM test ORDER BY id").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 2);

        Ok(())
    }

    #[test]
    async fn test_session_autocommit_mode() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("INSERT INTO test VALUES (1, 100)").await?;

        assert!(!session.in_transaction());

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
    async fn test_session_begin_implicitly_commits_previous() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("BEGIN").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;

        session.execute("BEGIN").await?;
        assert!(session.in_transaction());

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
    async fn test_session_set_autocommit_off() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        assert!(session.autocommit());

        session.execute("SET autocommit = 0").await?;
        assert!(!session.autocommit());
        assert!(!session.in_transaction());

        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        assert!(session.in_transaction());

        session.execute("INSERT INTO test VALUES (2, 200)").await?;
        assert!(session.in_transaction());

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 0);

        session.execute("COMMIT").await?;
        assert!(!session.in_transaction());

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 2);

        Ok(())
    }

    #[test]
    async fn test_session_set_autocommit_on_commits() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("SET autocommit = 0").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        assert!(session.in_transaction());

        session.execute("SET autocommit = 1").await?;
        assert!(session.autocommit());
        assert!(!session.in_transaction());

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
    async fn test_session_autocommit_off_rollback() -> Result<(), DatabaseError> {
        use tempfile::TempDir;
        let temp_dir = TempDir::new().unwrap();
        let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("SET autocommit = 0").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        session.execute("ROLLBACK").await?;

        assert!(!session.in_transaction());

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;
        assert_eq!(count, 0);

        Ok(())
    }

    #[test]
    async fn test_memory_storage_no_rollback() -> Result<(), DatabaseError> {
        let mut database = DataBaseBuilder::path(".").build_in_memory().await?;
        database
            .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
            .await?;

        let mut session = Session::new(&database);
        session.execute("BEGIN").await?;
        session.execute("INSERT INTO test VALUES (1, 100)").await?;
        session.execute("ROLLBACK").await?;

        let mut iter = database.run("SELECT * FROM test").await?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ()).await?.is_some() {
            count += 1;
        }
        iter.done().await?;

        assert_eq!(
            count, 1,
            "MemoryStorage does not support rollback - changes are immediately visible"
        );

        Ok(())
    }
}
