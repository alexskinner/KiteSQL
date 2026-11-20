// Integration test for Session transaction control
// Transaction control (BEGIN, COMMIT, ROLLBACK, SET autocommit) is available
// through the Session API.

#![cfg(any(feature = "lmdb", feature = "slatedb"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use kite_sql_serde_macros::maybe_async;
use tempfile::TempDir;

#[maybe_async]
#[test]
async fn test_session_transaction_control_integration() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE tx_test (id INT PRIMARY KEY, val INT)")
        .await?;

    let mut session = database.session();

    // Test 1: Autocommit mode (default)
    session
        .run("INSERT INTO tx_test VALUES (1, 100)")
        .await?
        .done()
        .await?;

    let mut iter = database.run("SELECT * FROM tx_test").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Autocommit should commit immediately");

    // Test 2: Explicit BEGIN/COMMIT
    session.run("BEGIN").await?.done().await?;
    assert!(session.in_transaction(), "Should be in transaction mode");
    session
        .run("INSERT INTO tx_test VALUES (2, 200)")
        .await?
        .done()
        .await?;
    session.run("COMMIT").await?.done().await?;
    assert!(
        !session.in_transaction(),
        "Should not be in transaction after COMMIT"
    );

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 2, "Explicit transaction should commit");

    // Test 3: SET autocommit = 0 with explicit COMMIT
    session.run("SET autocommit = 0").await?.done().await?;
    assert!(!session.autocommit(), "autocommit should be off");

    session
        .run("INSERT INTO tx_test VALUES (3, 300)")
        .await?
        .done()
        .await?;
    session.run("COMMIT").await?.done().await?;

    let mut iter = database.run("SELECT * FROM tx_test ORDER BY id").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 3, "autocommit=0 with explicit COMMIT works");

    // Test 4: SET autocommit = 1 commits any open transaction
    session
        .run("INSERT INTO tx_test VALUES (4, 400)")
        .await?
        .done()
        .await?;
    session.run("SET autocommit = 1").await?.done().await?;
    assert!(session.autocommit(), "autocommit should be on");

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 4").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "SET autocommit=1 should commit");

    // Test 5: BEGIN while transaction open should implicitly commit (MySQL behavior)
    session.run("BEGIN").await?.done().await?;
    session
        .run("INSERT INTO tx_test VALUES (10, 1000)")
        .await?
        .done()
        .await?;
    session.run("BEGIN").await?.done().await?;

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 10").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(
        count, 1,
        "BEGIN while transaction open should implicitly commit"
    );

    // Test 6: ROLLBACK discards changes
    session.run("BEGIN").await?.done().await?;
    session
        .run("INSERT INTO tx_test VALUES (99, 999)")
        .await?
        .done()
        .await?;
    session.run("ROLLBACK").await?.done().await?;

    let mut iter = database.run("SELECT * FROM tx_test WHERE id = 99").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 0, "ROLLBACK should discard changes");

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_earlier_row_survives() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

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

    {
        let iter = session
            .run("INSERT INTO test VALUES (2, 200), (1, 999)")
            .await?;
        let result = iter.done().await;
        assert!(
            result.is_err(),
            "Second insert should fail on duplicate key"
        );
    }

    let mut iter = session.run("SELECT * FROM test ORDER BY id").await?;
    let mut results = Vec::new();
    while let Some((id, val)) = iter
        .next_tuple(|_, row| {
            (
                row.get(0).unwrap().i32().unwrap(),
                row.get(1).unwrap().i32().unwrap(),
            )
        })
        .await?
    {
        results.push((id, val));
    }
    iter.done().await?;

    assert_eq!(results.len(), 1, "Should have 1 row");
    assert_eq!(results[0], (1, 100), "Original row should remain");

    session.run("COMMIT").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_unique_index_preserved() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT UNIQUE)")
        .await?;
    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    {
        let iter = session.run("INSERT INTO test VALUES (2, 100)").await?;
        let result = iter.done().await;
        assert!(result.is_err(), "Duplicate unique value should fail");
    }

    let mut iter = session.run("SELECT * FROM test WHERE val = 100").await?;
    let mut count = 0;
    let mut id: Option<i32> = None;
    while let Some(row_id) = iter.next_tuple(|_, row| row.get(0).unwrap().i32()).await? {
        count += 1;
        id = row_id;
    }
    iter.done().await?;

    assert_eq!(count, 1, "Should find original row by index");
    assert_eq!(id, Some(1), "Should be original row");

    {
        let iter = session.run("INSERT INTO test VALUES (3, 100)").await?;
        let result = iter.done().await;
        assert!(result.is_err(), "Duplicate should still be rejected");
    }

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_update_values_and_indexes() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT UNIQUE)")
        .await?;

    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (3, 300)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    {
        let iter = session.run("UPDATE test SET val = 100").await?;
        let result = iter.done().await;
        assert!(result.is_err(), "Update violating unique should fail");
    }

    let mut iter = session.run("SELECT val FROM test ORDER BY id").await?;
    let mut vals = Vec::new();
    while let Some(val) = iter.next_tuple(|_, row| row.get(0).unwrap().i32()).await? {
        if let Some(v) = val {
            vals.push(v);
        }
    }
    iter.done().await?;

    assert_eq!(vals, vec![100, 200, 300], "Values should be unchanged");

    let mut iter = session.run("SELECT id FROM test WHERE val = 200").await?;
    let id = iter
        .next_tuple(|_, row| row.get(0).unwrap().i32())
        .await?
        .expect("Index lookup should find row")
        .expect("Should have value");
    iter.done().await?;
    assert_eq!(id, 2, "Index lookup should return correct row");

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_pk_changing_update() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;

    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (3, 300)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    {
        let iter = session.run("UPDATE test SET id = 1").await?;
        let result = iter.done().await;
        assert!(
            result.is_err(),
            "PK-changing update creating duplicate should fail"
        );
    }

    let mut iter = session.run("SELECT id, val FROM test ORDER BY id").await?;
    let mut results = Vec::new();
    while let Some((id, val)) = iter
        .next_tuple(|_, row| {
            (
                row.get(0).unwrap().i32().unwrap(),
                row.get(1).unwrap().i32().unwrap(),
            )
        })
        .await?
    {
        results.push((id, val));
    }
    iter.done().await?;

    assert_eq!(
        results,
        vec![(1, 100), (2, 200), (3, 300)],
        "All rows should be restored"
    );

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_after_failed_statement() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

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

    {
        let iter = session.run("INSERT INTO test VALUES (1, 200)").await?;
        let result = iter.done().await;
        assert!(result.is_err(), "Duplicate key should fail");
    }

    assert!(
        session.in_transaction(),
        "Transaction should remain open after failed statement"
    );

    session
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;

    let mut iter = session.run("SELECT * FROM test ORDER BY id").await?;
    let mut results = Vec::new();
    while let Some((id, val)) = iter
        .next_tuple(|_, row| {
            (
                row.get(0).unwrap().i32().unwrap(),
                row.get(1).unwrap().i32().unwrap(),
            )
        })
        .await?
    {
        results.push((id, val));
    }
    iter.done().await?;

    assert_eq!(
        results,
        vec![(1, 100), (2, 200)],
        "Both inserts should be visible"
    );

    session.run("COMMIT").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_preserves_original_error() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;
    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    let iter = session.run("UPDATE test SET id = 1").await?;
    let result = iter.done().await;

    assert!(
        result.is_err(),
        "PK-changing update creating duplicate should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("DuplicatePrimaryKey") || err_msg.contains("duplicate"),
        "Error should mention the original constraint violation, got: {}",
        err_msg
    );

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_insert() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    // Insert one valid row
    session
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;

    // Multi-row insert that fails on row 2 (duplicate key)
    // MySQL: failing statement should be rolled back, transaction stays open
    let iter = session.run("INSERT INTO test VALUES (11,11),(1,1)").await?;
    // Error surfaces during done()/iteration, not run()
    let result = iter.done().await;
    assert!(
        result.is_err(),
        "Duplicate key should fail during iteration"
    );

    // Transaction should still be open
    assert!(session.in_transaction());

    // The id 11 should NOT be in the table (statement rolled back)
    let mut iter = session.run("SELECT * FROM test WHERE id = 11").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(
        count, 0,
        "Failed statement should be rolled back (id 11 should not exist)"
    );

    // The first row should still be there
    let mut iter = session.run("SELECT * FROM test WHERE id = 1").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Previous successful insert should remain");

    session.run("COMMIT").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_update() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT UNIQUE)")
        .await?;

    // Insert test data
    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (3, 300)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    // Update that fails partway (trying to set val=100 which violates UNIQUE constraint on row 2)
    let iter = session.run("UPDATE test SET val = 100").await?;
    // Error surfaces during done()/iteration, not run()
    let result = iter.done().await;
    assert!(
        result.is_err(),
        "UNIQUE constraint should fail during iteration"
    );

    // Transaction should still be open
    assert!(session.in_transaction());

    // Check that NO updates were applied (statement rolled back)
    let mut iter = session.run("SELECT val FROM test ORDER BY id").await?;
    let mut vals = Vec::new();
    while let Some(val) = iter
        .next_tuple(|_, row| row.get(0).unwrap().clone())
        .await?
    {
        vals.push(val);
    }
    iter.done().await?;

    assert_eq!(vals.len(), 3, "Should have 3 rows");
    assert_eq!(vals[0].i32().unwrap(), 100, "First value should be 100");
    assert_eq!(vals[1].i32().unwrap(), 200, "Second value should be 200");
    assert_eq!(vals[2].i32().unwrap(), 300, "Third value should be 300");

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_delete() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;

    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (2, 200)")
        .await?
        .done()
        .await?;
    database
        .run("INSERT INTO test VALUES (3, 300)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    session.run("BEGIN").await?.done().await?;

    {
        let result = session
            .run("DELETE FROM test WHERE val / (id - 2) > 0")
            .await;
        let failed = match result {
            Ok(iter) => iter.done().await.is_err(),
            Err(_) => true,
        };
        assert!(failed, "DELETE with division by zero should fail");
    }

    assert!(session.in_transaction(), "Transaction should still be open");

    let mut iter = session.run("SELECT COUNT(*) FROM test").await?;
    let count = iter
        .next_tuple(|_, row| row.get(0).unwrap().i32().unwrap())
        .await?
        .unwrap();
    iter.done().await?;
    assert_eq!(count, 3, "All rows should remain (statement rolled back)");

    session.run("ROLLBACK").await?.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_statement_atomicity_autocommit() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;

    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;

    let mut session = database.session();
    assert!(session.autocommit(), "Should be in autocommit mode");

    let iter = session
        .run("INSERT INTO test VALUES (11, 11), (1, 1)")
        .await?;
    let result = iter.done().await;
    assert!(
        result.is_err(),
        "Duplicate key should fail during iteration"
    );

    let mut iter = session.run("SELECT * FROM test WHERE id = 11").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(
        count, 0,
        "Failed statement in autocommit should leave no changes"
    );

    let mut iter = session.run("SELECT * FROM test WHERE id = 1").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Original row should remain");

    Ok(())
}

#[maybe_async]
#[test]
async fn test_new_transaction_gets_statement_atomicity() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, val INT)")
        .await?;
    database
        .run("INSERT INTO test VALUES (1, 100)")
        .await?
        .done()
        .await?;

    // Use plain new_transaction API (not Session)
    let mut tx = database.new_transaction(false)?;

    // Multi-row insert that fails on duplicate key
    let iter = tx.run("INSERT INTO test VALUES (2, 200), (1, 100)").await?;
    let result = iter.done().await;
    assert!(
        result.is_err(),
        "Duplicate key should fail during iteration"
    );

    // Verify statement was rolled back but transaction is still open
    let mut iter = tx.run("SELECT * FROM test WHERE id = 2").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(
        count, 0,
        "Failed INSERT should not have inserted row 2 (statement atomicity)"
    );

    // Verify row 1 still exists
    let mut iter = tx.run("SELECT * FROM test WHERE id = 1").await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Original row should remain");

    tx.commit().await?;

    Ok(())
}

#[maybe_async]
#[test]
async fn test_on_duplicate_key_update_cms_schema() -> Result<(), DatabaseError> {
    // Test the exact CMS system config use case:
    // - UUID varchar PRIMARY KEY
    // - UNIQUE (category, setting)
    // - Fresh UUID on every insert conflicts on UNIQUE index, not PK

    let temp_dir = TempDir::new().unwrap();
    let mut database = DataBaseBuilder::path(temp_dir.path())
        .build_test_db()
        .await?;

    database
        .ddl(
            "CREATE TABLE cms_system_config (
            id VARCHAR(50) PRIMARY KEY, 
            category VARCHAR(50), 
            setting VARCHAR(50), 
            value VARCHAR(100), 
            UNIQUE(category, setting)
        )",
        )
        .await?;

    let mut session = database.session();

    // First insert
    session
        .run(
            "INSERT INTO cms_system_config (id, category, setting, value) 
         VALUES ('uuid-001', 'system', 'boot_complete', 'false')",
        )
        .await?
        .done()
        .await?;

    // Verify initial row exists
    let mut iter = session
        .run(
            "SELECT COUNT(*) FROM cms_system_config 
         WHERE category = 'system' AND setting = 'boot_complete'",
        )
        .await?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        count += 1;
    }
    iter.done().await?;
    assert_eq!(count, 1, "Initial insert should create one row");

    // Second insert with fresh UUID but same (category, setting)
    // This should conflict on UNIQUE index and update existing row
    session
        .run(
            "INSERT INTO cms_system_config (id, category, setting, value) 
         VALUES ('uuid-002', 'system', 'boot_complete', 'should_be_ignored') 
         ON DUPLICATE KEY UPDATE value = 'true'",
        )
        .await?
        .done()
        .await?;

    // Verify only one row exists
    let mut iter = session
        .run("SELECT COUNT(*) FROM cms_system_config")
        .await?;
    let mut total = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        total += 1;
    }
    iter.done().await?;
    assert_eq!(total, 1, "Should still have exactly one row after ODKU");

    // Verify value was updated and id is still the original
    let mut iter = session
        .run(
            "SELECT id, value FROM cms_system_config 
         WHERE category = 'system' AND setting = 'boot_complete'",
        )
        .await?;
    let mut found = false;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        found = true;
    }
    iter.done().await?;
    assert!(found, "Should find the updated row");

    // Check updated value
    let mut iter = session
        .run("SELECT id FROM cms_system_config WHERE value = 'true'")
        .await?;
    let mut id_count = 0;
    while iter.next_tuple(|_, _| ()).await?.is_some() {
        id_count += 1;
    }
    iter.done().await?;
    assert_eq!(id_count, 1, "Value should be updated to 'true'");

    Ok(())
}
