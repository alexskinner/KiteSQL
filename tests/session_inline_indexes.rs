#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

#[test]
fn test_inline_indexes_in_session() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    let mut session = database.session();

    session.run("BEGIN")?.done()?;

    session
        .run("CREATE TABLE test_inline_idx (id INT PRIMARY KEY, a INT, b VARCHAR(50), KEY idx_a (a), INDEX (b))")?
        .done()?;

    session
        .run("INSERT INTO test_inline_idx VALUES (1, 10, 'first')")?
        .done()?;
    session
        .run("INSERT INTO test_inline_idx VALUES (2, 20, 'second')")?
        .done()?;

    let mut iter = session.run("SELECT * FROM test_inline_idx WHERE a = 10")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Index on column 'a' should work");

    let mut iter = session.run("SELECT * FROM test_inline_idx WHERE b = 'second'")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Unnamed index on column 'b' should work");

    session
        .run("INSERT INTO test_inline_idx VALUES (3, 30, 'third')")?
        .done()?;

    session.run("COMMIT")?.done()?;

    let mut iter = database.run("SELECT COUNT(*) FROM test_inline_idx")?;
    if let Some(row) = iter.next_tuple(|_, row| row.clone())? {
        assert_eq!(row[0].to_string(), "3", "Three rows should be committed");
    }
    iter.done()?;

    Ok(())
}

#[test]
fn test_fulltext_spatial_indexes() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database
        .ddl("CREATE TABLE test_ft_spatial (id INT PRIMARY KEY, text_col VARCHAR(100), geo_col INT, FULLTEXT KEY ft (text_col), SPATIAL KEY sk (geo_col))")?;

    database
        .run("INSERT INTO test_ft_spatial VALUES (1, 'fulltext search', 42)")?
        .done()?;

    let mut iter =
        database.run("SELECT * FROM test_ft_spatial WHERE text_col = 'fulltext search'")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "FULLTEXT index should be created as normal index");

    Ok(())
}

#[test]
fn test_unnamed_index_name_generation() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database
        .ddl("CREATE TABLE test_unnamed (id INT PRIMARY KEY, col1 INT, col2 INT, INDEX (col1), INDEX (col1), KEY (col2))")?;

    database
        .run("INSERT INTO test_unnamed VALUES (1, 10, 20)")?
        .done()?;

    let mut iter = database.run("SELECT * FROM test_unnamed WHERE col1 = 10")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(
        count, 1,
        "Multiple unnamed indexes on same column should work"
    );

    Ok(())
}
