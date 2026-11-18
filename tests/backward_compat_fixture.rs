//! Opens an LMDB database created by main (fc6ca71) with this build.
#![cfg(all(feature = "lmdb", feature = "parser"))]

use kite_sql::db::{CatalogKind, DataBaseBuilder};
use kite_sql::errors::DatabaseError;
use kite_sql::types::value::DataValue;
use std::path::{Path, PathBuf};

fn fixture_copy() -> tempfile::TempDir {
    let src: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/main_fc6ca71_db");
    let dst = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(&src).unwrap() {
        let e = e.unwrap();
        if e.file_name() == "lock.mdb" { continue; }
        std::fs::copy(e.path(), dst.path().join(e.file_name())).unwrap();
    }
    dst
}

#[test]
fn test_open_main_created_database() -> Result<(), DatabaseError> {
    let dir = fixture_copy();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.load(CatalogKind::Table("users".into()))?;
    db.load(CatalogKind::Table("orders".into()))?;

    let mut it = db.run("SELECT id, name, age FROM users ORDER BY id")?;
    let mut rows = vec![];
    while let Some(r) = it.next_tuple(|_, t| t.iter().cloned().collect::<Vec<_>>())? { rows.push(r); }
    it.done()?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][0], DataValue::Int32(2));
    assert_eq!(rows[1][1], DataValue::from("Bob".to_string()));

    let mut it = db.run("SELECT id FROM users WHERE name = 'Charlie'")?;
    assert_eq!(it.next_tuple(|_, t| t[0].clone())?.unwrap(), DataValue::Int32(3));
    it.done()?;

    let mut it = db.run("DESCRIBE users")?;
    let first = it.next_tuple(|_, t| t[1].to_string())?.unwrap();
    assert_eq!(first, "Integer");
    it.done()?;

    db.run("INSERT INTO users VALUES (4, 'Dora', 41, 1.00, true)")?.done()?;
    db.ddl("ALTER TABLE users ADD COLUMN note VARCHAR(10)")?;
    Ok(())
}
