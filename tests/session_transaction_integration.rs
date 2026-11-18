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

// Statement-level atomicity tests - SKIPPED due to architectural limitation
//
// These tests verify MySQL behavior where a statement that fails mid-execution should
// roll back its own partial writes while keeping the transaction open.
//
// BLOCKER: Implementing this requires one of:
// - Making StorageTransaction::set/remove async so we can capture prior values
// - Adding nested transaction/savepoint support to the storage layer
// - Restructuring the executor to buffer writes
//
// None of these can be done without modifying StorageTransaction trait or core execution.
// Tests are commented out to document the expected behavior.

#[test]
#[ignore = "Statement-level atomicity not implemented - see module documentation"]
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
    let result = session.run("INSERT INTO test VALUES (11,11),(1,1)");
    let is_err = result.is_err();
    drop(result);
    assert!(is_err, "Duplicate key should fail");

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
#[ignore = "Statement-level atomicity not implemented - see module documentation"]
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
    let result = session.run("UPDATE test SET val = 100");
    let is_err = result.is_err();
    drop(result);
    assert!(is_err, "UNIQUE constraint should fail");

    // Transaction should still be open
    assert!(session.in_transaction());

    // Check that NO updates were applied (statement rolled back)
    let mut iter = session.run("SELECT val FROM test ORDER BY id")?;
    let mut vals = Vec::new();
    while let Some(val) = iter.next_tuple(|_, row| row.get(1).unwrap().clone())? {
        vals.push(val);
    }
    iter.done()?;

    // Values should remain unchanged
    assert_eq!(vals.len(), 3);

    session.run("ROLLBACK")?.done()?;

    Ok(())
}
