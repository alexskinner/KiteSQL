// A Session's DDL: visible to its own later statements, published to the Database on COMMIT,
// discarded on ROLLBACK, and invisible to other sessions while uncommitted.

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
use kite_sql_serde_macros::maybe_async;
use tempfile::TempDir;

#[maybe_async]
async fn rows<I: ResultIter>(mut it: I) -> Result<Vec<String>, DatabaseError> {
    let mut out = vec![];
    while let Some(row) = it
        .next_tuple(|_, r| {
            r.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("|")
        })
        .await?
    {
        out.push(row);
    }
    it.done().await?;
    Ok(out)
}

#[maybe_async]
macro_rules! exec {
    ($s:expr, $sql:expr) => {
        $s.run($sql).await?.done()
    };
}

#[maybe_async]
#[test]
async fn create_insert_select_in_one_transaction_then_commit_publishes() -> Result<(), DatabaseError>
{
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "CREATE TABLE t (id INT PRIMARY KEY, v INT UNIQUE)").await?;
        exec!(s, "INSERT INTO t VALUES (1, 10), (2, 20)").await?;
        assert_eq!(
            rows(s.run("SELECT id, v FROM t ORDER BY id").await?).await?,
            ["1|10", "2|20"]
        );
        // a statement failing midway rolls back alone; the DDL and earlier rows stay
        assert!(exec!(s, "INSERT INTO t VALUES (3, 30), (4, 10)")
            .await
            .is_err());
        assert_eq!(
            rows(s.run("SELECT id FROM t ORDER BY id").await?).await?,
            ["1", "2"]
        );
        // DDL after DDL in the same transaction
        exec!(s, "ALTER TABLE t ADD COLUMN w INT").await?;
        exec!(s, "CREATE INDEX w_idx ON t (w)").await?;
        assert_eq!(
            rows(s.run("SELECT id, w FROM t ORDER BY id").await?).await?,
            ["1|NULL", "2|NULL"]
        );
        exec!(s, "COMMIT").await?;
    }
    // published: the database sees it with no `load`
    assert_eq!(
        rows(db.run("SELECT id, v FROM t ORDER BY id").await?).await?,
        ["1|10", "2|20"]
    );
    assert_eq!(rows(db.run("DESCRIBE t").await?).await?.len(), 3);
    Ok(())
}

#[maybe_async]
#[test]
async fn rollback_discards_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE keep (id INT PRIMARY KEY, a INT)")
        .await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "CREATE TABLE gone (id INT PRIMARY KEY)").await?;
        exec!(s, "INSERT INTO gone VALUES (1)").await?;
        exec!(s, "ALTER TABLE keep ADD COLUMN b INT").await?;
        exec!(s, "DROP TABLE keep").await?;
        assert!(
            s.run("SELECT * FROM keep").await.is_err(),
            "dropped inside the txn"
        );
        exec!(s, "ROLLBACK").await?;
        // the session is back on the shared catalog
        assert!(s.run("SELECT * FROM gone").await.is_err());
        assert_eq!(rows(s.run("DESCRIBE keep").await?).await?.len(), 2);
    }
    assert!(db.run("SELECT * FROM gone").await.is_err());
    assert_eq!(rows(db.run("DESCRIBE keep").await?).await?.len(), 2);
    // and a later &mut DDL still works on the untouched catalog
    db.ddl("ALTER TABLE keep ADD COLUMN c INT").await?;
    assert_eq!(rows(db.run("DESCRIBE keep").await?).await?.len(), 3);
    Ok(())
}

#[maybe_async]
#[test]
async fn other_sessions_do_not_see_uncommitted_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    let mut a = db.session();
    let mut b = db.session();
    exec!(a, "BEGIN").await?;
    exec!(a, "CREATE TABLE t (id INT PRIMARY KEY)").await?;
    exec!(a, "INSERT INTO t VALUES (1)").await?;
    assert!(
        b.run("SELECT * FROM t").await.is_err(),
        "not visible to another session"
    );
    assert!(
        db.run("SELECT * FROM t").await.is_err(),
        "not visible to the database"
    );
    exec!(a, "COMMIT").await?;
    assert_eq!(rows(b.run("SELECT id FROM t").await?).await?, ["1"]);
    assert_eq!(rows(db.run("SELECT id FROM t").await?).await?, ["1"]);
    Ok(())
}

#[maybe_async]
#[test]
async fn running_statement_keeps_its_generation_across_a_publish() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)").await?;
    db.run("INSERT INTO t VALUES (1, 1), (2, 2)")
        .await?
        .done()
        .await?;
    let mut old = db.run("SELECT id, a FROM t ORDER BY id").await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "ALTER TABLE t ADD COLUMN b INT").await?;
        exec!(s, "COMMIT").await?;
    }
    let mut got = vec![];
    while let Some(r) = old
        .next_tuple(|_, r| {
            r.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("|")
        })
        .await?
    {
        got.push(r);
    }
    old.done().await?;
    assert_eq!(got, ["1|1", "2|2"]);
    assert_eq!(
        rows(db.run("SELECT * FROM t ORDER BY id").await?).await?,
        ["1|1|NULL", "2|2|NULL"]
    );
    // folded into the base catalog by the next &mut call
    db.ddl("CREATE TABLE u (id INT PRIMARY KEY)").await?;
    assert_eq!(rows(db.run("DESCRIBE t").await?).await?.len(), 3);
    Ok(())
}

#[maybe_async]
#[test]
async fn many_publishes_then_mut_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    for i in 0..5 {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, &format!("CREATE TABLE t{i} (id INT PRIMARY KEY)")).await?;
        exec!(s, "COMMIT").await?;
    }
    for i in 0..5 {
        assert_eq!(
            rows(db.run(format!("DESCRIBE t{i}")).await?).await?.len(),
            1
        );
    }
    db.ddl("DROP TABLE t2").await?;
    assert!(db.run("SELECT * FROM t2").await.is_err());
    assert_eq!(rows(db.run("DESCRIBE t4").await?).await?.len(), 1);
    Ok(())
}

#[maybe_async]
#[test]
async fn table_functions_and_views_survive_private_and_published_catalogs(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "CREATE TABLE t (id INT PRIMARY KEY)").await?;
        exec!(
            s,
            "INSERT INTO t SELECT CAST(number AS INT) FROM TABLE(numbers(3))"
        )
        .await?;
        exec!(s, "CREATE VIEW v AS SELECT id FROM t WHERE id > 0").await?;
        assert_eq!(
            rows(s.run("SELECT id FROM v ORDER BY id").await?).await?,
            ["1", "2"]
        );
        exec!(s, "COMMIT").await?;
    }
    assert_eq!(
        rows(db.run("SELECT id FROM v ORDER BY id").await?).await?,
        ["1", "2"]
    );
    assert_eq!(
        rows(db.run("SELECT number FROM TABLE(numbers(2))").await?).await?,
        ["0", "1"]
    );
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "DROP VIEW v").await?;
        exec!(s, "ROLLBACK").await?;
    }
    assert_eq!(
        rows(db.run("SELECT id FROM v ORDER BY id").await?).await?,
        ["1", "2"]
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn long_running_transaction_keeps_consistent_generation_across_ddl(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)").await?;
    db.run("INSERT INTO t VALUES (1, 10), (2, 20)")
        .await?
        .done()
        .await?;
    // Use read-only transaction to avoid LMDB single-writer lock
    let mut tx = db.new_transaction(true)?;
    let first_select = rows(tx.run("SELECT id, a FROM t ORDER BY id").await?).await?;
    assert_eq!(first_select, ["1|10", "2|20"]);
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "ALTER TABLE t ADD COLUMN b INT").await?;
        exec!(s, "INSERT INTO t VALUES (3, 30, 300)").await?;
        exec!(s, "COMMIT").await?;
    }
    // Transaction still sees original 2-column schema
    let after_ddl = rows(tx.run("SELECT * FROM t ORDER BY id").await?).await?;
    assert_eq!(after_ddl, ["1|10", "2|20"]);
    tx.commit().await?;
    // New query sees 3-column schema with new row
    let final_rows = rows(db.run("SELECT * FROM t ORDER BY id").await?).await?;
    assert_eq!(final_rows, ["1|10|NULL", "2|20|NULL", "3|30|300"]);
    Ok(())
}

#[maybe_async]
#[test]
async fn iterator_from_database_keeps_consistent_generation_across_publish(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, x INT)").await?;
    db.run("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)")
        .await?
        .done()
        .await?;
    let mut long_iter = db.run("SELECT id, x FROM t ORDER BY id").await?;
    assert_eq!(
        long_iter
            .next_tuple(|_, r| {
                r.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .await?,
        Some("1|100".to_string())
    );
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "ALTER TABLE t ADD COLUMN y INT").await?;
        exec!(s, "INSERT INTO t VALUES (4, 400, 4000)").await?;
        exec!(s, "COMMIT").await?;
    }
    let remaining = {
        let mut rows = vec![];
        while let Some(r) = long_iter
            .next_tuple(|_, r| {
                r.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .await?
        {
            rows.push(r);
        }
        rows
    };
    assert_eq!(remaining, ["2|200", "3|300"]);
    long_iter.done().await?;
    let new_gen = rows(db.run("SELECT * FROM t ORDER BY id").await?).await?;
    assert_eq!(
        new_gen,
        ["1|100|NULL", "2|200|NULL", "3|300|NULL", "4|400|4000"]
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn read_only_transaction_pins_catalog_and_survives_schema_mismatch(
) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20))")
        .await?;
    db.run("INSERT INTO t VALUES (1, 'Alice'), (2, 'Bob')")
        .await?
        .done()
        .await?;
    let mut tx = db.new_transaction(true)?;
    let before = rows(tx.run("SELECT id, name FROM t ORDER BY id").await?).await?;
    assert_eq!(before, ["1|Alice", "2|Bob"]);
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "ALTER TABLE t ADD COLUMN age INT").await?;
        exec!(s, "COMMIT").await?;
    }
    let after = rows(tx.run("SELECT id, name FROM t ORDER BY id").await?).await?;
    assert_eq!(after, ["1|Alice", "2|Bob"]);
    tx.commit().await?;
    let latest = rows(db.run("SELECT * FROM t ORDER BY id").await?).await?;
    assert_eq!(latest, ["1|Alice|NULL", "2|Bob|NULL"]);
    Ok(())
}
#[maybe_async]
#[cfg(all(feature = "orm", feature = "macros"))]
#[test]
async fn orm_in_read_only_transaction_pins_catalog() -> Result<(), DatabaseError> {
    use kite_sql::Model;

    #[derive(Model, Debug, Clone, PartialEq)]
    #[model(table = "person")]
    struct PersonNew {
        #[model(primary_key)]
        id: i32,
        name: String,
        age: Option<i32>,
    }
    #[derive(Model, Debug, Clone, PartialEq)]
    #[model(table = "person")]
    struct Person {
        #[model(primary_key)]
        id: i32,
        name: String,
    }

    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.create_table::<Person>().await?;
    db.insert(&Person {
        id: 1,
        name: "Alice".into(),
    })
    .await?;

    let mut tx = db.new_transaction(true)?;

    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "ALTER TABLE person ADD COLUMN age INT").await?;
        exec!(s, "COMMIT").await?;
    }

    let person = tx.get::<Person>(&1).await?.unwrap();
    assert_eq!(
        person,
        Person {
            id: 1,
            name: "Alice".into()
        }
    );

    let new = tx.get::<PersonNew>(&1).await;
    println!(
        "NEWSTRUCT {:?}",
        new.as_ref().map(|_| ()).map_err(|e| format!("{e:?} / {e}"))
    );
    assert!(matches!(new, Err(ref e) if !matches!(e, DatabaseError::IO(_))));
    tx.commit().await?;
    Ok(())
}

#[maybe_async]
#[cfg(feature = "parser")]
#[test]
async fn program_from_different_database_returns_foreign_program_error() -> Result<(), DatabaseError>
{
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();
    let mut db1 = DataBaseBuilder::path(dir1.path()).build_test_db().await?;
    let mut db2 = DataBaseBuilder::path(dir2.path()).build_test_db().await?;

    db1.ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .await?;
    db2.ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .await?;
    db1.run("INSERT INTO t VALUES (1, 100)")
        .await?
        .done()
        .await?;
    db2.run("INSERT INTO t VALUES (2, 200)")
        .await?
        .done()
        .await?;

    let mut program = db1.program_sql("SELECT * FROM t", &[])?;
    let mut tx = db2.new_transaction(false)?;

    let result = program.execute(&mut tx, []).await;
    assert!(matches!(result, Err(DatabaseError::ForeignProgram)));
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE TABLE t2 (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE TABLE t3 (id INT PRIMARY KEY)").await?;
    db.ddl("DROP TABLE t1, t2, t3").await?;
    assert!(db.run("SELECT * FROM t1").await.is_err());
    assert!(db.run("SELECT * FROM t2").await.is_err());
    assert!(db.run("SELECT * FROM t3").await.is_err());
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables_if_exists() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE TABLE t2 (id INT PRIMARY KEY)").await?;
    db.ddl("DROP TABLE IF EXISTS t1, t2, t_nonexistent").await?;
    assert!(db.run("SELECT * FROM t1").await.is_err());
    assert!(db.run("SELECT * FROM t2").await.is_err());
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables_without_if_exists_fails_on_missing() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE TABLE t2 (id INT PRIMARY KEY)").await?;
    let result = db.ddl("DROP TABLE t1, t2, t_nonexistent").await;
    assert!(result.is_err());
    assert_eq!(
        rows(db.run("SELECT * FROM t1").await?).await?,
        Vec::<String>::new()
    );
    assert_eq!(
        rows(db.run("SELECT * FROM t2").await?).await?,
        Vec::<String>::new()
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables_with_duplicates() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
    let result = db.ddl("DROP TABLE t1, t1").await;
    assert!(result.is_err());
    assert_eq!(
        rows(db.run("SELECT * FROM t1").await?).await?,
        Vec::<String>::new()
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables_in_transaction() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
        exec!(s, "CREATE TABLE t2 (id INT PRIMARY KEY)").await?;
        exec!(s, "CREATE TABLE t3 (id INT PRIMARY KEY)").await?;
        exec!(s, "INSERT INTO t1 VALUES (1)").await?;
        exec!(s, "INSERT INTO t2 VALUES (2)").await?;
        exec!(s, "DROP TABLE t1, t2").await?;
        assert!(s.run("SELECT * FROM t1").await.is_err());
        assert!(s.run("SELECT * FROM t2").await.is_err());
        assert_eq!(
            rows(s.run("SELECT * FROM t3").await?).await?,
            Vec::<String>::new()
        );
        exec!(s, "COMMIT").await?;
    }
    assert!(db.run("SELECT * FROM t1").await.is_err());
    assert!(db.run("SELECT * FROM t2").await.is_err());
    assert_eq!(
        rows(db.run("SELECT * FROM t3").await?).await?,
        Vec::<String>::new()
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_tables_rollback() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t1 (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE TABLE t2 (id INT PRIMARY KEY)").await?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN").await?;
        exec!(s, "DROP TABLE t1, t2").await?;
        assert!(s.run("SELECT * FROM t1").await.is_err());
        assert!(s.run("SELECT * FROM t2").await.is_err());
        exec!(s, "ROLLBACK").await?;
    }
    assert_eq!(
        rows(db.run("SELECT * FROM t1").await?).await?,
        Vec::<String>::new()
    );
    assert_eq!(
        rows(db.run("SELECT * FROM t2").await?).await?,
        Vec::<String>::new()
    );
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_views() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE VIEW v1 AS SELECT id FROM t").await?;
    db.ddl("CREATE VIEW v2 AS SELECT id FROM t").await?;
    db.ddl("DROP VIEW v1, v2").await?;
    assert!(db.run("SELECT * FROM v1").await.is_err());
    assert!(db.run("SELECT * FROM v2").await.is_err());
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_views_if_exists() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE VIEW v1 AS SELECT id FROM t").await?;
    db.ddl("DROP VIEW IF EXISTS v1, v_nonexistent").await?;
    assert!(db.run("SELECT * FROM v1").await.is_err());
    Ok(())
}

#[maybe_async]
#[test]
async fn drop_multiple_views_with_duplicates() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY)").await?;
    db.ddl("CREATE VIEW v1 AS SELECT id FROM t").await?;
    let result = db.ddl("DROP VIEW v1, v1").await;
    assert!(result.is_err());
    assert_eq!(
        rows(db.run("SELECT * FROM v1").await?).await?,
        Vec::<String>::new()
    );
    Ok(())
}
