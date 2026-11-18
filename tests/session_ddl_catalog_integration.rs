// A Session's DDL: visible to its own later statements, published to the Database on COMMIT,
// discarded on ROLLBACK, and invisible to other sessions while uncommitted.

#![cfg(all(not(feature = "async"), feature = "lmdb", feature = "parser"))]

use kite_sql::db::{DataBaseBuilder, ResultIter};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

fn rows<I: ResultIter>(mut it: I) -> Result<Vec<String>, DatabaseError> {
    let mut out = vec![];
    while let Some(row) = it.next_tuple(|_, r| {
        r.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|")
    })? {
        out.push(row);
    }
    it.done()?;
    Ok(out)
}

macro_rules! exec {
    ($s:expr, $sql:expr) => {
        $s.run($sql)?.done()
    };
}

#[test]
fn create_insert_select_in_one_transaction_then_commit_publishes() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "CREATE TABLE t (id INT PRIMARY KEY, v INT UNIQUE)")?;
        exec!(s, "INSERT INTO t VALUES (1, 10), (2, 20)")?;
        assert_eq!(
            rows(s.run("SELECT id, v FROM t ORDER BY id")?)?,
            ["1|10", "2|20"]
        );
        // a statement failing midway rolls back alone; the DDL and earlier rows stay
        assert!(exec!(s, "INSERT INTO t VALUES (3, 30), (4, 10)").is_err());
        assert_eq!(rows(s.run("SELECT id FROM t ORDER BY id")?)?, ["1", "2"]);
        // DDL after DDL in the same transaction
        exec!(s, "ALTER TABLE t ADD COLUMN w INT")?;
        exec!(s, "CREATE INDEX w_idx ON t (w)")?;
        assert_eq!(
            rows(s.run("SELECT id, w FROM t ORDER BY id")?)?,
            ["1|NULL", "2|NULL"]
        );
        exec!(s, "COMMIT")?;
    }
    // published: the database sees it with no `load`
    assert_eq!(
        rows(db.run("SELECT id, v FROM t ORDER BY id")?)?,
        ["1|10", "2|20"]
    );
    assert_eq!(rows(db.run("DESCRIBE t")?)?.len(), 3);
    Ok(())
}

#[test]
fn rollback_discards_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE keep (id INT PRIMARY KEY, a INT)")?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "CREATE TABLE gone (id INT PRIMARY KEY)")?;
        exec!(s, "INSERT INTO gone VALUES (1)")?;
        exec!(s, "ALTER TABLE keep ADD COLUMN b INT")?;
        exec!(s, "DROP TABLE keep")?;
        assert!(
            s.run("SELECT * FROM keep").is_err(),
            "dropped inside the txn"
        );
        exec!(s, "ROLLBACK")?;
        // the session is back on the shared catalog
        assert!(s.run("SELECT * FROM gone").is_err());
        assert_eq!(rows(s.run("DESCRIBE keep")?)?.len(), 2);
    }
    assert!(db.run("SELECT * FROM gone").is_err());
    assert_eq!(rows(db.run("DESCRIBE keep")?)?.len(), 2);
    // and a later &mut DDL still works on the untouched catalog
    db.ddl("ALTER TABLE keep ADD COLUMN c INT")?;
    assert_eq!(rows(db.run("DESCRIBE keep")?)?.len(), 3);
    Ok(())
}

#[test]
fn other_sessions_do_not_see_uncommitted_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    let mut a = db.session();
    let mut b = db.session();
    exec!(a, "BEGIN")?;
    exec!(a, "CREATE TABLE t (id INT PRIMARY KEY)")?;
    exec!(a, "INSERT INTO t VALUES (1)")?;
    assert!(
        b.run("SELECT * FROM t").is_err(),
        "not visible to another session"
    );
    assert!(
        db.run("SELECT * FROM t").is_err(),
        "not visible to the database"
    );
    exec!(a, "COMMIT")?;
    assert_eq!(rows(b.run("SELECT id FROM t")?)?, ["1"]);
    assert_eq!(rows(db.run("SELECT id FROM t")?)?, ["1"]);
    Ok(())
}

#[test]
fn running_statement_keeps_its_generation_across_a_publish() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)")?;
    db.run("INSERT INTO t VALUES (1, 1), (2, 2)")?.done()?;
    let mut old = db.run("SELECT id, a FROM t ORDER BY id")?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "ALTER TABLE t ADD COLUMN b INT")?;
        exec!(s, "COMMIT")?;
    }
    let mut got = vec![];
    while let Some(r) = old.next_tuple(|_, r| {
        r.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|")
    })? {
        got.push(r);
    }
    old.done()?;
    assert_eq!(got, ["1|1", "2|2"]);
    assert_eq!(
        rows(db.run("SELECT * FROM t ORDER BY id")?)?,
        ["1|1|NULL", "2|2|NULL"]
    );
    // folded into the base catalog by the next &mut call
    db.ddl("CREATE TABLE u (id INT PRIMARY KEY)")?;
    assert_eq!(rows(db.run("DESCRIBE t")?)?.len(), 3);
    Ok(())
}

#[test]
fn many_publishes_then_mut_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    for i in 0..5 {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, &format!("CREATE TABLE t{i} (id INT PRIMARY KEY)"))?;
        exec!(s, "COMMIT")?;
    }
    for i in 0..5 {
        assert_eq!(rows(db.run(format!("DESCRIBE t{i}"))?)?.len(), 1);
    }
    db.ddl("DROP TABLE t2")?;
    assert!(db.run("SELECT * FROM t2").is_err());
    assert_eq!(rows(db.run("DESCRIBE t4")?)?.len(), 1);
    Ok(())
}

#[test]
fn table_functions_and_views_survive_private_and_published_catalogs() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "CREATE TABLE t (id INT PRIMARY KEY)")?;
        exec!(
            s,
            "INSERT INTO t SELECT CAST(number AS INT) FROM TABLE(numbers(3))"
        )?;
        exec!(s, "CREATE VIEW v AS SELECT id FROM t WHERE id > 0")?;
        assert_eq!(rows(s.run("SELECT id FROM v ORDER BY id")?)?, ["1", "2"]);
        exec!(s, "COMMIT")?;
    }
    assert_eq!(rows(db.run("SELECT id FROM v ORDER BY id")?)?, ["1", "2"]);
    assert_eq!(
        rows(db.run("SELECT number FROM TABLE(numbers(2))")?)?,
        ["0", "1"]
    );
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "DROP VIEW v")?;
        exec!(s, "ROLLBACK")?;
    }
    assert_eq!(rows(db.run("SELECT id FROM v ORDER BY id")?)?, ["1", "2"]);
    Ok(())
}

#[test]
fn long_running_transaction_keeps_consistent_generation_across_ddl() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, a INT)")?;
    db.run("INSERT INTO t VALUES (1, 10), (2, 20)")?.done()?;
    // Use read-only transaction to avoid LMDB single-writer lock
    let mut tx = db.new_transaction(true)?;
    let first_select = rows(tx.run("SELECT id, a FROM t ORDER BY id")?)?;
    assert_eq!(first_select, ["1|10", "2|20"]);
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "ALTER TABLE t ADD COLUMN b INT")?;
        exec!(s, "INSERT INTO t VALUES (3, 30, 300)")?;
        exec!(s, "COMMIT")?;
    }
    // Transaction still sees original 2-column schema
    let after_ddl = rows(tx.run("SELECT * FROM t ORDER BY id")?)?;
    assert_eq!(after_ddl, ["1|10", "2|20"]);
    tx.commit()?;
    // New query sees 3-column schema with new row
    let final_rows = rows(db.run("SELECT * FROM t ORDER BY id")?)?;
    assert_eq!(final_rows, ["1|10|NULL", "2|20|NULL", "3|30|300"]);
    Ok(())
}

#[test]
fn iterator_from_database_keeps_consistent_generation_across_publish() -> Result<(), DatabaseError>
{
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, x INT)")?;
    db.run("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)")?
        .done()?;
    let mut long_iter = db.run("SELECT id, x FROM t ORDER BY id")?;
    assert_eq!(
        long_iter.next_tuple(|_, r| {
            r.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("|")
        })?,
        Some("1|100".to_string())
    );
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "ALTER TABLE t ADD COLUMN y INT")?;
        exec!(s, "INSERT INTO t VALUES (4, 400, 4000)")?;
        exec!(s, "COMMIT")?;
    }
    let remaining = {
        let mut rows = vec![];
        while let Some(r) = long_iter.next_tuple(|_, r| {
            r.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("|")
        })? {
            rows.push(r);
        }
        rows
    };
    assert_eq!(remaining, ["2|200", "3|300"]);
    long_iter.done()?;
    let new_gen = rows(db.run("SELECT * FROM t ORDER BY id")?)?;
    assert_eq!(
        new_gen,
        ["1|100|NULL", "2|200|NULL", "3|300|NULL", "4|400|4000"]
    );
    Ok(())
}

#[test]
fn read_only_transaction_pins_catalog_and_survives_schema_mismatch() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20))")?;
    db.run("INSERT INTO t VALUES (1, 'Alice'), (2, 'Bob')")?
        .done()?;
    let mut tx = db.new_transaction(true)?;
    let before = rows(tx.run("SELECT id, name FROM t ORDER BY id")?)?;
    assert_eq!(before, ["1|Alice", "2|Bob"]);
    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "ALTER TABLE t ADD COLUMN age INT")?;
        exec!(s, "COMMIT")?;
    }
    let after = rows(tx.run("SELECT id, name FROM t ORDER BY id")?)?;
    assert_eq!(after, ["1|Alice", "2|Bob"]);
    tx.commit()?;
    let latest = rows(db.run("SELECT * FROM t ORDER BY id")?)?;
    assert_eq!(latest, ["1|Alice|NULL", "2|Bob|NULL"]);
    Ok(())
}
#[cfg(all(feature = "orm", feature = "macros"))]
#[test]
fn orm_in_read_only_transaction_pins_catalog() -> Result<(), DatabaseError> {
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
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.create_table::<Person>()?;
    db.insert(&Person {
        id: 1,
        name: "Alice".into(),
    })?;

    let mut tx = db.new_transaction(true)?;

    {
        let mut s = db.session();
        exec!(s, "BEGIN")?;
        exec!(s, "ALTER TABLE person ADD COLUMN age INT")?;
        exec!(s, "COMMIT")?;
    }

    let person = tx.get::<Person>(&1)?.unwrap();
    assert_eq!(
        person,
        Person {
            id: 1,
            name: "Alice".into()
        }
    );

    let new = tx.get::<PersonNew>(&1);
    println!("NEWSTRUCT {:?}", new.as_ref().map(|_| ()).map_err(|e| format!("{e:?} / {e}")));
    assert!(matches!(new, Err(ref e) if !matches!(e, DatabaseError::IO(_))));
    tx.commit()?;
    Ok(())
}

#[cfg(feature = "parser")]
#[test]
fn program_from_different_database_returns_foreign_program_error() -> Result<(), DatabaseError> {
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();
    let mut db1 = DataBaseBuilder::path(dir1.path()).build_lmdb()?;
    let mut db2 = DataBaseBuilder::path(dir2.path()).build_lmdb()?;

    db1.ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT)")?;
    db2.ddl("CREATE TABLE t (id INT PRIMARY KEY, v INT)")?;
    db1.run("INSERT INTO t VALUES (1, 100)")?.done()?;
    db2.run("INSERT INTO t VALUES (2, 200)")?.done()?;

    let mut program = db1.program_sql("SELECT * FROM t", &[])?;
    let mut tx = db2.new_transaction(false)?;

    let result = program.execute(&mut tx, []);
    assert!(matches!(result, Err(DatabaseError::ForeignProgram)));
    Ok(())
}
