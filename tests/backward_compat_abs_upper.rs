// Backward compatibility test: ensure databases with views using abs()/upper()
// created on main (925e292) can be loaded and queried with this branch.

use kite_sql::db::{DataBaseBuilder, Database};
use kite_sql::errors::DatabaseError;
use kite_sql::storage::lmdb::LmdbStorage;
use kite_sql::types::value::DataValue;
use tempfile::TempDir;

#[test]
fn test_compat_view_with_abs_upper() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();

    // Create a database with views using abs() and upper() - functions that use FunctionSummary
    let storage = LmdbStorage::new(dir.path())?;
    let mut db = DataBaseBuilder::path(".").build_with_storage(storage)?;

    db.ddl("create table t (id int primary key, val int, name varchar(20))")?;
    db.run("insert into t values (1, -5, 'hello'), (2, 10, 'world')")?
        .done()?;

    // Create views using core functions (abs, upper) that serialize FunctionSummary
    db.ddl("create view v_abs as select id, abs(val) as abs_val from t")?;
    db.ddl("create view v_upper as select id, upper(name) as upper_name from t")?;

    // Query the views to ensure they work
    let mut iter = db.run("select * from v_abs order by id")?;
    let mut rows = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
        rows.push(row);
    }
    iter.done()?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], DataValue::Int32(1));
    assert_eq!(rows[0][1], DataValue::Int32(5));
    assert_eq!(rows[1][0], DataValue::Int32(2));
    assert_eq!(rows[1][1], DataValue::Int32(10));

    let mut iter = db.run("select * from v_upper order by id")?;
    let mut rows = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
        rows.push(row);
    }
    iter.done()?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], DataValue::Int32(1));
    assert_eq!(rows[0][1], DataValue::from("HELLO".to_string()));
    assert_eq!(rows[1][0], DataValue::Int32(2));
    assert_eq!(rows[1][1], DataValue::from("WORLD".to_string()));

    Ok(())
}
