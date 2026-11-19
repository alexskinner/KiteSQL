#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{CatalogKind, ConsumeMut, DataBaseBuilder, TransactionIter};
use kite_sql::errors::DatabaseError;
use kite_sql::planner::PlanArena;
use tempfile::TempDir;

struct Consume;
impl<S: kite_sql::storage::Storage> ConsumeMut<S> for Consume {
    type Output = ();
    fn consume<'a>(
        self,
        _iter: &mut TransactionIter<'a, 'a, S::TransactionType<'a>, PlanArena<'a>>,
    ) -> Result<Self::Output, DatabaseError> {
        Ok(())
    }
}

#[test]
fn test_inline_indexes_persist_after_reopen() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;

        database.ddl(
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
        )?;

        database
            .run("INSERT INTO t VALUES (1, 10, 'first', 100)")?
            .done()?;
        database
            .run("INSERT INTO t VALUES (2, 20, 'second', 200)")?
            .done()?;
    }

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;
        database.load(CatalogKind::Table("t".into()))?;

        {
            let result = database.run_mut("INSERT INTO t VALUES (3, 30, 'first', 100)", Consume);
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
            let result = database.run_mut("INSERT INTO t VALUES (3, 30, 'third', 100)", Consume);
            match result {
                Err(DatabaseError::DuplicateUniqueValue) => {}
                Err(e) => panic!(
                    "Expected DuplicateUniqueValue for single-column UNIQUE KEY uk_c, got error: {}",
                    e
                ),
                Ok(_) => panic!("Expected DuplicateUniqueValue for single-column UNIQUE KEY uk_c, got Ok"),
            }
        }

        database.ddl("DROP INDEX t.idx_a")?;
        database.ddl("DROP INDEX t.idx_b")?;
        database.ddl("DROP INDEX t.uk_bc")?;
        database.ddl("DROP INDEX t.uk_c")?;

        Ok(())
    }
}

#[test]
fn test_prefix_lengths_accepted() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl(
        "CREATE TABLE t (
            id INT PRIMARY KEY,
            text_col VARCHAR(200),
            KEY idx_prefix (text_col(10)),
            UNIQUE KEY uk_prefix (text_col(20))
        )",
    )?;

    database
        .run("INSERT INTO t VALUES (1, 'a long text value')")?
        .done()?;

    let mut iter = database.run("SELECT * FROM t WHERE text_col = 'a long text value'")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Index with prefix length should work");

    Ok(())
}

#[test]
fn test_named_single_column_unique() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.run_mut(
        "CREATE TABLE t (id INT PRIMARY KEY, val INT, UNIQUE KEY uk_val (val))",
        Consume,
    )?;
    database.run_mut("INSERT INTO t VALUES (1, 100)", Consume)?;

    {
        let result = database.run_mut("INSERT INTO t VALUES (2, 100)", Consume);
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

    database.run_mut("DROP INDEX t.uk_val", Consume)?;

    Ok(())
}

#[test]
fn test_named_single_column_unique_persist() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;
        database.ddl("CREATE TABLE t (id INT PRIMARY KEY, val INT, UNIQUE KEY uk_val (val))")?;
        database.run("INSERT INTO t VALUES (1, 100)")?.done()?;
    }

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;
        database.load(CatalogKind::Table("t".into()))?;

        {
            let result = database.run_mut("INSERT INTO t VALUES (2, 100)", Consume);
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

        database.ddl("DROP INDEX t.uk_val")?;
    }

    Ok(())
}
