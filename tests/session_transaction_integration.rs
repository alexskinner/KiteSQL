// Integration test for Session transaction control
// Transaction control (BEGIN, COMMIT, ROLLBACK, SET autocommit) is only available
// through the Session API, not through Database::run() directly.

#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{DataBaseBuilder, Session};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

#[test]
fn test_session_transaction_control_integration() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path()).build_lmdb()?;

    database.ddl("CREATE TABLE tx_test (id INT PRIMARY KEY, val INT)")?;

    let mut session = Session::new(&database);

    session.run("INSERT INTO tx_test VALUES (1, 100)")?;

    let mut iter = database.run("SELECT * FROM tx_test")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 1, "Autocommit should commit immediately");

    session.run("BEGIN")?;
    session.run("INSERT INTO tx_test VALUES (2, 200)")?;
    session.run("COMMIT")?;

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 2, "Explicit transaction should commit");

    session.run("SET autocommit = 0")?;
    session.run("INSERT INTO tx_test VALUES (3, 300)")?;
    session.run("COMMIT")?;

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 3, "autocommit=0 with explicit COMMIT works");

    session.run("SET autocommit = 1")?;

    session.run("BEGIN")?;
    session.run("INSERT INTO tx_test VALUES (10, 1000)")?;
    session.run("BEGIN")?;

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

    session.run("ROLLBACK")?;

    Ok(())
}
