// Catalog generations published by Session commits: concurrent publishers must not lose each
// other's DDL, and a prepared program runs only in a transaction pinned to the generation it
// was planned against. The memory bound itself is a unit test (`db::catalog_generation_tests`),
// as it counts live generations through a test-only counter.

#![cfg(all(any(feature = "lmdb", feature = "slatedb"), feature = "parser"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use kite_sql::types::value::DataValue;
use kite_sql::types::LogicalType;
use kite_sql_serde_macros::maybe_async;
use tempfile::TempDir;

#[maybe_async]
macro_rules! exec {
    ($s:expr, $sql:expr) => {
        $s.run($sql).await?.done().await
    };
}

#[maybe_async]
async fn column_count<I: ResultIter>(iter: I) -> Result<usize, DatabaseError> {
    let count = iter.schema(|schema| schema.len());
    iter.done().await?;
    Ok(count)
}

// Needs a storage that serialises concurrent writers (LMDB): SlateDB's optimistic transactions
// reject overlapping DDL commits with a conflict instead of queueing them.
#[cfg(feature = "lmdb")]
#[maybe_async]
#[test]
async fn concurrent_session_ddl_publishes_every_table() -> Result<(), DatabaseError> {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 125;
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;

    std::thread::scope(|scope| {
        let workers = (0..THREADS)
            .map(|thread| {
                let db = &db;
                scope.spawn(move || -> Result<(), DatabaseError> {
                    crate::maybe_async::block_on(async move {
                        for i in 0..PER_THREAD {
                            let mut session = db.session();
                            exec!(session, "BEGIN")?;
                            exec!(
                                session,
                                &format!("CREATE TABLE t_{thread}_{i} (id INT PRIMARY KEY)")
                            )?;
                            exec!(session, "COMMIT")?;
                        }
                        Ok::<(), DatabaseError>(())
                    })
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("worker panicked")?;
        }
        Ok::<_, DatabaseError>(())
    })?;

    // Binding resolves names in the newest published generation: a table a lost publish
    // dropped would fail here with "table not found".
    let mut session = db.session();
    let mut missing = Vec::new();
    for thread in 0..THREADS {
        for i in 0..PER_THREAD {
            let name = format!("t_{thread}_{i}");
            if async {
                session
                    .run(&format!("SELECT * FROM {name}"))
                    .await?
                    .done()
                    .await
            }
            .await
            .is_err()
            {
                missing.push(name);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} tables missing: {missing:?}",
        missing.len()
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn program_prepared_in_pinned_transaction_runs_there_after_a_publish(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)").await?;
    db.run("INSERT INTO t VALUES (1, 10)").await?.done().await?;
    let db = db;

    let mut tx = db.new_transaction(true)?;
    {
        let mut session = db.session();
        exec!(session, "BEGIN")?;
        exec!(session, "ALTER TABLE t ADD COLUMN b INT")?;
        exec!(session, "COMMIT")?;
    }
    // Prepared after the publish, but bound to (and recorded as) the transaction's generation.
    let mut program = tx.program_sql(
        "SELECT * FROM t WHERE id = $1",
        &[(1, LogicalType::Integer)],
    )?;
    for _ in 0..2 {
        let iter = program
            .execute(&mut tx, [(1, DataValue::Int32(1).into())])
            .await?;
        assert_eq!(column_count(iter).await?, 2);
    }
    tx.commit().await?;

    // A new transaction pins the newer generation: the program is stale there.
    let mut tx = db.new_transaction(true)?;
    let result = program
        .execute(&mut tx, [(1, DataValue::Int32(1).into())])
        .await;
    assert!(matches!(result, Err(DatabaseError::StaleCatalog)));
    Ok(())
}

#[maybe_async]
#[test]
async fn program_prepared_before_a_publish_is_stale_in_a_later_transaction(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)").await?;
    db.run("INSERT INTO t VALUES (1, 10)").await?.done().await?;
    let db = db;

    let mut program = db.program_sql("SELECT * FROM t", &[])?;
    // A transaction opened before the publish shares the program's generation.
    let mut early = db.new_transaction(true)?;
    {
        let mut session = db.session();
        exec!(session, "BEGIN")?;
        exec!(session, "ALTER TABLE t ADD COLUMN b INT")?;
        exec!(session, "COMMIT")?;
    }
    assert_eq!(
        column_count(program.execute(&mut early, []).await?).await?,
        2
    );
    early.commit().await?;

    let mut late = db.new_transaction(true)?;
    assert!(matches!(
        program.execute(&mut late, []).await,
        Err(DatabaseError::StaleCatalog)
    ));
    // Re-preparing picks up the new generation.
    let mut program = db.program_sql("SELECT * FROM t", &[])?;
    assert_eq!(
        column_count(program.execute(&mut late, []).await?).await?,
        3
    );
    Ok(())
}

// Needs two overlapping write transactions: the sync path's memory storage or SlateDB. On the
// async path with LMDB, `build_in_memory` is LMDB, whose single writer would wait forever.
#[cfg(any(not(feature = "async"), not(feature = "lmdb")))]
#[maybe_async]
#[test]
async fn session_transaction_binds_against_its_pinned_generation() -> Result<(), DatabaseError> {
    // Memory storage lets two write transactions overlap, so another session can publish
    // while this one is open (LMDB's single writer would block it).
    let mut db = DataBaseBuilder::path(".").build_in_memory().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)").await?;
    let db = db;

    let mut reader = db.session();
    exec!(reader, "BEGIN")?;
    assert_eq!(column_count(reader.run("SELECT * FROM t").await?).await?, 2);
    {
        let mut writer = db.session();
        exec!(writer, "BEGIN")?;
        exec!(writer, "ALTER TABLE t ADD COLUMN b INT")?;
        exec!(writer, "COMMIT")?;
    }
    // Still the generation pinned at BEGIN, not the one just published.
    assert_eq!(column_count(reader.run("SELECT * FROM t").await?).await?, 2);
    exec!(reader, "COMMIT")?;
    assert_eq!(column_count(reader.run("SELECT * FROM t").await?).await?, 3);
    Ok(())
}
