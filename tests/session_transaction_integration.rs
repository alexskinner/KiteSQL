// Integration test for Session transaction control
// Transaction control (BEGIN, COMMIT, ROLLBACK, SET autocommit) is available
// through the Session API.

#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

#[test]
fn test_session_transaction_control_integration() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE tx_test (id INT PRIMARY KEY, val INT)")?;

    let mut session = database.session();

    // Test 1: Autocommit mode (default)
    session.run("INSERT INTO tx_test VALUES (1, 100)")?.done()?;

    let mut iter = database.run("SELECT * FROM tx_test")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Autocommit should commit immediately");

    // Test 2: Explicit BEGIN/COMMIT
    session.run("BEGIN")?.done()?;
    assert!(session.in_transaction(), "Should be in transaction mode");
    session.run("INSERT INTO tx_test VALUES (2, 200)")?.done()?;
    session.run("COMMIT")?.done()?;
    assert!(
        !session.in_transaction(),
        "Should not be in transaction after COMMIT"
    );

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 2, "Explicit transaction should commit");

    // Test 3: SET autocommit = 0 with explicit COMMIT
    session.run("SET autocommit = 0")?.done()?;
    assert!(!session.autocommit(), "autocommit should be off");

    session.run("INSERT INTO tx_test VALUES (3, 300)")?.done()?;
    session.run("COMMIT")?.done()?;

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 3, "autocommit=0 with explicit COMMIT works");

    // Test 4: SET autocommit = 1 commits any open transaction
    session.run("INSERT INTO tx_test VALUES (4, 400)")?.done()?;
    session.run("SET autocommit = 1")?.done()?;
    assert!(session.autocommit(), "autocommit should be on");

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 4")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "SET autocommit=1 should commit");

    // Test 5: BEGIN while transaction open should implicitly commit (MySQL behavior)
    session.run("BEGIN")?.done()?;
    session
        .run("INSERT INTO tx_test VALUES (10, 1000)")?
        .done()?;
    session.run("BEGIN")?.done()?;

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 10")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(
        count, 1,
        "BEGIN while transaction open should implicitly commit"
    );

    // Test 6: ROLLBACK discards changes
    session.run("BEGIN")?.done()?;
    session
        .run("INSERT INTO tx_test VALUES (99, 999)")?
        .done()?;
    session.run("ROLLBACK")?.done()?;

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 99")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 0, "ROLLBACK should discard changes");

    Ok(())
}

#[test]
fn test_statement_atomicity_insert() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")?;

    let mut session = database.session();
    session.run("BEGIN")?.done()?;

    // Insert one valid row
    session.run("INSERT INTO test VALUES (1, 100)")?.done()?;

    // Multi-row insert that fails on row 2 (duplicate key)
    // MySQL: failing statement should be rolled back, transaction stays open
    let iter = session.run("INSERT INTO test VALUES (11,11),(1,1)")?;
    // Error surfaces during done()/iteration, not run()
    let result = iter.done();
    assert!(
        result.is_err(),
        "Duplicate key should fail during iteration"
    );

    // Transaction should still be open
    assert!(session.in_transaction());

    // The id 11 should NOT be in the table (statement rolled back)
    let mut iter = session.run("SELECT * FROM test WHERE id = 11")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(
        count, 0,
        "Failed statement should be rolled back (id 11 should not exist)"
    );

    // The first row should still be there
    let mut iter = session.run("SELECT * FROM test WHERE id = 1")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Previous successful insert should remain");

    session.run("COMMIT")?.done()?;

    Ok(())
}

#[test]
fn test_statement_atomicity_update() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT UNIQUE)")?;

    // Insert test data
    database.run("INSERT INTO test VALUES (1, 100)")?.done()?;
    database.run("INSERT INTO test VALUES (2, 200)")?.done()?;
    database.run("INSERT INTO test VALUES (3, 300)")?.done()?;

    let mut session = database.session();
    session.run("BEGIN")?.done()?;

    // Update that fails partway (trying to set val=100 which violates UNIQUE constraint on row 2)
    let iter = session.run("UPDATE test SET val = 100")?;
    // Error surfaces during done()/iteration, not run()
    let result = iter.done();
    assert!(
        result.is_err(),
        "UNIQUE constraint should fail during iteration"
    );

    // Transaction should still be open
    assert!(session.in_transaction());

    // Check that NO updates were applied (statement rolled back)
    let mut iter = session.run("SELECT val FROM test ORDER BY id")?;
    let mut vals = Vec::new();
    while let Some(val) = iter.next_tuple(|_, row| row.get(0).unwrap().clone())? {
        vals.push(val);
    }
    iter.done()?;

    assert_eq!(vals.len(), 3, "Should have 3 rows");
    assert_eq!(vals[0].i32().unwrap(), 100, "First value should be 100");
    assert_eq!(vals[1].i32().unwrap(), 200, "Second value should be 200");
    assert_eq!(vals[2].i32().unwrap(), 300, "Third value should be 300");

    session.run("ROLLBACK")?.done()?;

    Ok(())
}

#[test]
fn test_statement_atomicity_delete() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")?;

    database.run("INSERT INTO test VALUES (1, 100)")?.done()?;
    database.run("INSERT INTO test VALUES (2, 200)")?.done()?;
    database.run("INSERT INTO test VALUES (3, 300)")?.done()?;

    let mut session = database.session();
    session.run("BEGIN")?.done()?;

    {
        let result = session.run("DELETE FROM test WHERE val / (id - 2) > 0");
        let failed = match result {
            Ok(iter) => iter.done().is_err(),
            Err(_) => true,
        };
        assert!(failed, "DELETE with division by zero should fail");
    }

    assert!(session.in_transaction(), "Transaction should still be open");

    let mut iter = session.run("SELECT COUNT(*) FROM test")?;
    let count = iter
        .next_tuple(|_, row| row.get(0).unwrap().i32().unwrap())?
        .unwrap();
    iter.done()?;
    assert_eq!(count, 3, "All rows should remain (statement rolled back)");

    session.run("ROLLBACK")?.done()?;

    Ok(())
}

#[test]
fn test_statement_atomicity_autocommit() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")?;

    database.run("INSERT INTO test VALUES (1, 100)")?.done()?;

    let mut session = database.session();
    assert!(session.autocommit(), "Should be in autocommit mode");

    let iter = session.run("INSERT INTO test VALUES (11, 11), (1, 1)")?;
    let result = iter.done();
    assert!(
        result.is_err(),
        "Duplicate key should fail during iteration"
    );

    let mut iter = session.run("SELECT * FROM test WHERE id = 11")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(
        count, 0,
        "Failed statement in autocommit should leave no changes"
    );

    let mut iter = session.run("SELECT * FROM test WHERE id = 1")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Original row should remain");

    Ok(())
}
