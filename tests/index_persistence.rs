#![cfg(any(feature = "lmdb", feature = "slatedb"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
#[cfg(feature = "lmdb")]
use kite_sql::db::CatalogKind;
use kite_sql::db::{ConsumeMut, DataBaseBuilder, TransactionIter};
use kite_sql::errors::DatabaseError;
use kite_sql::planner::PlanArena;
use kite_sql_serde_macros::maybe_async;
use tempfile::TempDir;

struct Consume;
#[maybe_async]
impl<S: kite_sql::storage::Storage> ConsumeMut<S> for Consume {
    type Output = ();
    async fn consume<'a>(
        self,
        _iter: &mut TransactionIter<'a, 'a, S::TransactionType<'a>, PlanArena<'a>>,
    ) -> Result<Self::Output, DatabaseError> {
        Ok(())
    }
}

// Reopens the same directory, so it needs a persistent storage.
#[cfg(feature = "lmdb")]
#[maybe_async]
#[test]
async fn test_inline_indexes_persist_after_reopen() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    {
        let mut database = DataBaseBuilder::path(db_path).build_test_db().await?;

        database
            .ddl(
                "CREATE TABLE t (
                id INT PRIMARY KEY,
                a INT,
                b VARCHAR(50),
                c INT,
                KEY idx_a (a),
                INDEX idx_b (b),
                UNIQUE KEY uk_bc (b, c),
                UNIQUE KEY uk_c (c)
            )",
            )
            .await?;

        database
            .run("INSERT INTO t VALUES (1, 10, 'first', 100)")
            .await?
            .done()
            .await?;
        database
            .run("INSERT INTO t VALUES (2, 20, 'second', 200)")
            .await?
            .done()
            .await?;
    }

    {
        let mut database = DataBaseBuilder::path(db_path).build_test_db().await?;
        database.load(CatalogKind::Table("t".into())).await?;

        {
            let result = database
                .run_mut("INSERT INTO t VALUES (3, 30, 'first', 100)", Consume)
                .await;
            match result {
                Err(DatabaseError::DuplicateUniqueValue) => {}
                Err(e) => panic!(
                    "Expected DuplicateUniqueValue for composite UNIQUE KEY uk_bc, got error: {}",
                    e
                ),
                Ok(_) => {
                    panic!("Expected DuplicateUniqueValue for composite UNIQUE KEY uk_bc, got Ok")
                }
            }
        }

        {
            let result = database
                .run_mut("INSERT INTO t VALUES (3, 30, 'third', 100)", Consume)
                .await;
            match result {
                Err(DatabaseError::DuplicateUniqueValue) => {}
                Err(e) => panic!(
                    "Expected DuplicateUniqueValue for single-column UNIQUE KEY uk_c, got error: {}",
                    e
                ),
                Ok(_) => panic!("Expected DuplicateUniqueValue for single-column UNIQUE KEY uk_c, got Ok"),
            }
        }

        database.ddl("DROP INDEX t.idx_a").await?;
        database.ddl("DROP INDEX t.idx_b").await?;
        database.ddl("DROP INDEX t.uk_bc").await?;
        database.ddl("DROP INDEX t.uk_c").await?;

        Ok(())
    }
}

#[maybe_async]
#[test]
async fn test_prefix_lengths_accepted() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl(
            "CREATE TABLE t (
            id INT PRIMARY KEY,
            text_col VARCHAR(200),
            KEY idx_prefix (text_col(10)),
            UNIQUE KEY uk_prefix (text_col(20))
        )",
        )
        .await?;

    database
        .run("INSERT INTO t VALUES (1, 'a long text value')")
        .await?
        .done()
        .await?;

    let mut iter = database
        .run("SELECT * FROM t WHERE text_col = 'a long text value'")
        .await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Index with prefix length should work");

    Ok(())
}

#[maybe_async]
#[test]
async fn test_named_single_column_unique() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .run_mut(
            "CREATE TABLE t (id INT PRIMARY KEY, val INT, UNIQUE KEY uk_val (val))",
            Consume,
        )
        .await?;
    database
        .run_mut("INSERT INTO t VALUES (1, 100)", Consume)
        .await?;

    {
        let result = database
            .run_mut("INSERT INTO t VALUES (2, 100)", Consume)
            .await;
        match result {
            Err(DatabaseError::DuplicateUniqueValue) => {}
            Err(e) => panic!(
                "Expected DuplicateUniqueValue for named single-column UNIQUE KEY, got error: {}",
                e
            ),
            Ok(_) => {
                panic!("Expected DuplicateUniqueValue for named single-column UNIQUE KEY, got Ok")
            }
        }
    }

    database.run_mut("DROP INDEX t.uk_val", Consume).await?;

    Ok(())
}

// Reopens the same directory, so it needs a persistent storage.
#[cfg(feature = "lmdb")]
#[maybe_async]
#[test]
async fn test_named_single_column_unique_persist() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    {
        let mut database = DataBaseBuilder::path(db_path).build_test_db().await?;
        database
            .ddl("CREATE TABLE t (id INT PRIMARY KEY, val INT, UNIQUE KEY uk_val (val))")
            .await?;
        database
            .run("INSERT INTO t VALUES (1, 100)")
            .await?
            .done()
            .await?;
    }

    {
        let mut database = DataBaseBuilder::path(db_path).build_test_db().await?;
        database.load(CatalogKind::Table("t".into())).await?;

        {
            let result = database
                .run_mut("INSERT INTO t VALUES (2, 100)", Consume)
                .await;
            match result {
                Err(DatabaseError::DuplicateUniqueValue) => {}
                Err(e) => panic!(
                    "Expected DuplicateUniqueValue for named single-column UNIQUE KEY uk_val, got error: {}",
                    e
                ),
                Ok(_) => panic!(
                    "Expected DuplicateUniqueValue for named single-column UNIQUE KEY uk_val, got Ok"
                ),
            }
        }

        database.ddl("DROP INDEX t.uk_val").await?;
    }

    Ok(())
}
