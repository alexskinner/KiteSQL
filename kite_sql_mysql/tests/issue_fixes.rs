// Tests for issue fixes in PR #34

use kite_sql::db::{DataBaseBuilder, Database};
use kite_sql::errors::DatabaseError;
use kite_sql::storage::lmdb::LmdbStorage;
use kite_sql::types::value::DataValue;
use kite_sql_mysql::MySqlBuilderExt;
use tempfile::TempDir;

fn database(dir: &TempDir) -> Result<Database<LmdbStorage>, DatabaseError> {
    let storage = LmdbStorage::new(dir.path())?;
    DataBaseBuilder::path(".")
        .with_mysql()?
        .build_with_storage(storage)
}

fn scalar(db: &Database<LmdbStorage>, sql: &str) -> Result<DataValue, DatabaseError> {
    let mut iter = db.run(sql)?;
    let mut rows = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
        rows.push(row);
    }
    iter.done()?;
    Ok(rows.remove(0).remove(0))
}

// Issue 2: ROUND panics
#[test]
fn round_no_panic_on_large_negative_d() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select round(cast(123 as bigint), -10)")?,
        DataValue::Bigint(0)
    );
    assert_eq!(
        scalar(&db, "select round(2147483647, -1)")?,
        DataValue::Int32(0)
    );
    Ok(())
}

// Issue 3: HEX formatting
#[test]
fn hex_no_zero_padding() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select hex(255)")?,
        DataValue::from("FF".to_string())
    );
    assert_eq!(
        scalar(&db, "select hex(1)")?,
        DataValue::from("1".to_string())
    );
    Ok(())
}

#[test]
fn hex_rounds_floats() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, val double)")?;
    db.run("insert into t values (1, 2.5), (2, 2.4)")?.done()?;

    assert_eq!(
        scalar(&db, "select hex(val) from t where id = 1")?,
        DataValue::from("3".to_string())
    );
    assert_eq!(
        scalar(&db, "select hex(val) from t where id = 2")?,
        DataValue::from("2".to_string())
    );
    Ok(())
}

#[test]
fn hex_boolean() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, val boolean)")?;
    db.run("insert into t values (1, true), (2, false)")?
        .done()?;

    assert_eq!(
        scalar(&db, "select hex(val) from t where id = 1")?,
        DataValue::from("1".to_string())
    );
    assert_eq!(
        scalar(&db, "select hex(val) from t where id = 2")?,
        DataValue::from("0".to_string())
    );
    Ok(())
}

// Issue 4: No 10-argument cap
#[test]
fn concat_11_args() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(
            &db,
            "select concat('a','b','c','d','e','f','g','h','i','j','k')"
        )?,
        DataValue::from("abcdefghijk".to_string())
    );
    Ok(())
}

// Issue 5: GREATEST/LEAST mixed numeric types
#[test]
fn greatest_least_mixed_numeric() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, i int, b bigint, d double)")?;
    db.run("insert into t values (1, 5, 10, 7.5)")?.done()?;

    use ordered_float::OrderedFloat;
    assert_eq!(
        scalar(&db, "select greatest(i, b, d) from t")?,
        DataValue::Float64(OrderedFloat(10.0))
    );
    assert_eq!(
        scalar(&db, "select least(i, b, d) from t")?,
        DataValue::Float64(OrderedFloat(5.0))
    );
    Ok(())
}

// Issue 6: ROUND on any Decimal precision/scale
#[test]
fn round_decimal_any_precision() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl(
        "create table t (id int primary key, d1 decimal(6,3), d2 decimal(20,7), d3 decimal(5,0))",
    )?;
    db.run("insert into t values (1, 1.005, 1.0000005, 123)")?
        .done()?;

    use rust_decimal::Decimal;
    assert_eq!(
        scalar(&db, "select round(d1, 2) from t")?,
        DataValue::Decimal(Decimal::new(101, 2))
    );
    assert_eq!(
        scalar(&db, "select round(d2, 6) from t")?,
        DataValue::Decimal(Decimal::new(1000001, 6))
    );
    assert_eq!(
        scalar(&db, "select round(d3, 1) from t")?,
        DataValue::Decimal(Decimal::new(1230, 1))
    );
    Ok(())
}

#[test]
fn round_decimal_expressions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, d decimal(10,3), e decimal(10,3))")?;
    db.run("insert into t values (1, 3.00, 1.005)")?.done()?;

    use rust_decimal::Decimal;
    assert_eq!(
        scalar(&db, "select round(d*2, 2) from t")?,
        DataValue::Decimal(Decimal::new(600, 2))
    );
    assert_eq!(
        scalar(&db, "select round(d*e, 2) from t")?,
        DataValue::Decimal(Decimal::new(302, 2))
    );
    Ok(())
}

#[test]
fn round_null_handling() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select round(cast(null as double))")?,
        DataValue::Null
    );
    assert_eq!(
        scalar(&db, "select round(cast(1.5 as double), null)")?,
        DataValue::Null
    );
    Ok(())
}

// Issue 7: Boolean to text conversion
#[test]
fn boolean_to_text() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, val boolean)")?;
    db.run("insert into t values (1, true), (2, false)")?
        .done()?;

    assert_eq!(
        scalar(&db, "select length(val) from t where id = 1")?,
        DataValue::Int32(1)
    );
    assert_eq!(
        scalar(&db, "select concat(val, 'a') from t where id = 1")?,
        DataValue::from("1a".to_string())
    );
    assert_eq!(
        scalar(&db, "select concat(val, 'a') from t where id = 2")?,
        DataValue::from("0a".to_string())
    );
    Ok(())
}

// Issue 8: locate with pos < 1
#[test]
fn locate_pos_less_than_1() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select locate('a', 'abc', 0)")?,
        DataValue::Int32(0)
    );
    assert_eq!(
        scalar(&db, "select locate('a', 'abc', -1)")?,
        DataValue::Int32(0)
    );
    Ok(())
}
