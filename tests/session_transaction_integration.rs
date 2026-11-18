// Integration test for Session transaction control
// Transaction control (BEGIN, COMMIT, ROLLBACK, SET autocommit) is available
// through the Session API. Note that the safe implementation tracks transaction
// state but does not keep actual storage transactions open across calls.

#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{DataBaseBuilder, Session};
use kite_sql::errors::DatabaseError;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn test_session_transaction_control_integration() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE tx_test (id INT PRIMARY KEY, val INT)")?;

    let database = Arc::new(database);
    let mut session = Session::new(database.clone());

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

    // Test 6: ROLLBACK changes transaction state
    session.run("BEGIN")?.done()?;
    assert!(session.in_transaction());
    session.run("ROLLBACK")?.done()?;
    assert!(!session.in_transaction());

    Ok(())
}
