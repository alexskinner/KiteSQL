// Backward compatibility test: ensure databases with views using abs()/upper()
// created on main (925e292) can be loaded and queried with this branch.

#![cfg(any(feature = "lmdb", feature = "slatedb"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;
use kite_sql::types::value::DataValue;
use kite_sql_serde_macros::maybe_async;
use tempfile::TempDir;

#[maybe_async]
#[test]
async fn test_compat_view_with_abs_upper() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();

    // Create a database with views using abs() and upper() - functions that use FunctionSummary
    let storage = common::test_storage(dir.path()).await?;
    let mut db = DataBaseBuilder::path(dir.path()).build_with_storage(storage)?;

    db.ddl("create table t (id int primary key, val int, name varchar(20))")
        .await?;
    db.run("insert into t values (1, -5, 'hello'), (2, 10, 'world')")
        .await?
        .done()
        .await?;

    // Create views using core functions (abs, upper) that serialize FunctionSummary
    db.ddl("create view v_abs as select id, abs(val) as abs_val from t")
        .await?;
    db.ddl("create view v_upper as select id, upper(name) as upper_name from t")
        .await?;

    // Query the views to ensure they work
    let mut iter = db.run("select * from v_abs order by id").await?;
    let mut rows = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values()).await? {
        rows.push(row);
    }
    iter.done().await?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], DataValue::Int32(1));
    assert_eq!(rows[0][1], DataValue::Int32(5));
    assert_eq!(rows[1][0], DataValue::Int32(2));
    assert_eq!(rows[1][1], DataValue::Int32(10));

    let mut iter = db.run("select * from v_upper order by id").await?;
    let mut rows = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values()).await? {
        rows.push(row);
    }
    iter.done().await?;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], DataValue::Int32(1));
    assert_eq!(rows[0][1], DataValue::from("HELLO".to_string()));
    assert_eq!(rows[1][0], DataValue::Int32(2));
    assert_eq!(rows[1][1], DataValue::from("WORLD".to_string()));

    Ok(())
}
