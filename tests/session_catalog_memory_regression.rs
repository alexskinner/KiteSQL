// Regression test for catalog generation memory leak (PR #29 fix)
// Verifies that old catalog generations are automatically reclaimed when no longer needed.

#![cfg(all(not(feature = "async"), feature = "lmdb", feature = "parser"))]

use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

fn exec<S: kite_sql::storage::Storage>(
    s: &mut kite_sql::db::Session<'_, S>,
    sql: &str,
) -> Result<(), DatabaseError> {
    s.run(sql)?.done()
}

#[test]
fn catalog_generations_bounded_under_repeated_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;

    // Track generations as we go
    let mut max_gen_seen = 0;
    let mut gen_at_1000 = 0;

    // Run 2000+ DDL commits through a Session with no &mut Database call
    for i in 0..2050 {
        let mut s = db.session();
        exec(&mut s, "BEGIN")?;

        // Alternate between CREATE and DROP to exercise the catalog
        if i % 2 == 0 {
            exec(&mut s, &format!("CREATE TABLE t{i} (id INT PRIMARY KEY)"))?;
        } else {
            exec(&mut s, &format!("DROP TABLE t{}", i - 1))?;
        }

        exec(&mut s, "COMMIT")?;

        // Sample the generation number
        let current_gen = db.test_catalog_generation();
        max_gen_seen = max_gen_seen.max(current_gen);

        if i == 1000 {
            gen_at_1000 = current_gen;
        }
    }

    let final_gen = db.test_catalog_generation();

    // With the fix, generations are reclaimed automatically.
    // The generation counter increments, but old generations drop.
    // We just verify that generations are being created (counter increases).
    assert!(
        final_gen > gen_at_1000,
        "Generation counter should increase: gen_at_1000={gen_at_1000}, final_gen={final_gen}"
    );

    // The key test: after all this, the weak references to old generations should be gone.
    // We sample one generation now and verify it can be dropped.
    let weak_sample = db.test_catalog_weak_ref();

    // Create a new generation
    {
        let mut s = db.session();
        exec(&mut s, "BEGIN")?;
        exec(&mut s, "CREATE TABLE final_test (id INT PRIMARY KEY)")?;
        exec(&mut s, "COMMIT")?;
    }

    // The old generation should have been released
    assert_eq!(
        weak_sample.strong_count(),
        0,
        "Old generation should be dropped when not referenced"
    );

    Ok(())
}

#[test]
fn concurrent_sessions_see_latest_after_commit() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;

    // Session A creates a table
    {
        let mut a = db.session();
        exec(&mut a, "BEGIN")?;
        exec(&mut a, "CREATE TABLE t (id INT PRIMARY KEY)")?;
        exec(&mut a, "COMMIT")?;
    }

    // Session B should see the table
    {
        let mut b = db.session();
        exec(&mut b, "INSERT INTO t VALUES (1)")?;
        let mut iter = b.run("SELECT * FROM t")?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        assert_eq!(count, 1);
    }

    Ok(())
}

#[test]
fn program_detects_stale_catalog_after_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;

    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT)")?;
    let mut program = db.program_sql("SELECT * FROM t", &[])?;

    // Commit DDL through a session
    {
        let mut s = db.session();
        exec(&mut s, "BEGIN")?;
        exec(&mut s, "ALTER TABLE t ADD COLUMN w INT")?;
        exec(&mut s, "COMMIT")?;
    }

    // Program should detect the stale catalog
    let mut tx = db.new_transaction(false)?;
    match program.execute(&mut tx, []) {
        Err(kite_sql::errors::DatabaseError::StaleCatalog) => {}
        Ok(_) => panic!("Expected StaleCatalog error"),
        Err(e) => panic!("Expected StaleCatalog, got {:?}", e),
    }

    Ok(())
}

#[test]
fn pinned_transaction_sees_consistent_catalog() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;

    db.ddl("CREATE TABLE t (id INT PRIMARY KEY)")?;
    db.run("INSERT INTO t VALUES (1), (2)")?.done()?;

    // Start a read-only transaction
    let mut tx = db.new_transaction(true)?;
    let before = {
        let mut iter = tx.run("SELECT * FROM t ORDER BY id")?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        count
    };
    assert_eq!(before, 2);

    // Another session commits DDL
    {
        let mut s = db.session();
        exec(&mut s, "BEGIN")?;
        exec(&mut s, "ALTER TABLE t ADD COLUMN v INT")?;
        exec(&mut s, "INSERT INTO t VALUES (3, 30)")?;
        exec(&mut s, "COMMIT")?;
    }

    // Original transaction still sees old schema with 2 rows
    let after = {
        let mut iter = tx.run("SELECT * FROM t ORDER BY id")?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        count
    };
    assert_eq!(after, 2, "Transaction should see original catalog");

    tx.commit()?;

    // New query sees updated schema with 3 rows
    let latest = {
        let mut iter = db.run("SELECT * FROM t ORDER BY id")?;
        let mut count = 0;
        while iter.next_tuple(|_, _| ())?.is_some() {
            count += 1;
        }
        iter.done()?;
        count
    };
    assert_eq!(latest, 3);

    Ok(())
}

#[test]
fn alter_after_create_in_same_transaction() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;

    let mut s = db.session();
    exec(&mut s, "BEGIN")?;
    exec(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)")?;
    exec(&mut s, "INSERT INTO t VALUES (1)")?;
    exec(&mut s, "ALTER TABLE t ADD COLUMN v INT")?;
    exec(&mut s, "INSERT INTO t VALUES (2, 20)")?;

    let mut iter = s.run("SELECT * FROM t ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 2);

    exec(&mut s, "COMMIT")?;

    // Database sees the committed result
    let mut iter = db.run("SELECT * FROM t ORDER BY id")?;
    let mut count = 0;
    while iter.next_tuple(|_, _| ())?.is_some() {
        count += 1;
    }
    iter.done()?;
    assert_eq!(count, 2);

    Ok(())
}
