// Copyright 2024 KipData/KiteSQL
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for the MySQL function pack installed by `with_mysql()`.

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

fn rows(db: &Database<LmdbStorage>, sql: &str) -> Result<Vec<Vec<DataValue>>, DatabaseError> {
    let mut iter = db.run(sql)?;
    let mut out = Vec::new();
    while let Some(row) = iter.next_tuple(|_, row| row.take_values())? {
        out.push(row);
    }
    iter.done()?;
    Ok(out)
}

fn text(value: &str) -> DataValue {
    DataValue::from(value.to_string())
}

fn scalar(db: &Database<LmdbStorage>, sql: &str) -> Result<DataValue, DatabaseError> {
    Ok(rows(db, sql)?.remove(0).remove(0))
}

#[test]
fn replace_semantics() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select replace('a-b-c', '-', '+')")?,
        text("a+b+c")
    );
    assert_eq!(
        scalar(&db, "select replace('aaaa', 'aa', 'b')")?,
        text("bb")
    );
    assert_eq!(
        scalar(&db, "select replace('Abc abc ABC', 'abc', 'x')")?,
        text("Abc x ABC")
    );
    assert_eq!(scalar(&db, "select REPLACE('abc', 'b', 'B')")?, text("aBc"));
    assert_eq!(scalar(&db, "select Replace('abc', 'b', '')")?, text("ac"));
    assert_eq!(scalar(&db, "select replace('abc', '', 'x')")?, text("abc"));
    assert_eq!(scalar(&db, "select replace('abc', 'z', 'x')")?, text("abc"));
    assert_eq!(
        scalar(&db, "select replace(null, 'a', 'b')")?,
        DataValue::Null
    );
    assert_eq!(
        scalar(&db, "select replace('abc', null, 'b')")?,
        DataValue::Null
    );
    assert_eq!(
        scalar(&db, "select replace('abc', 'a', null)")?,
        DataValue::Null
    );
    assert_eq!(
        scalar(
            &db,
            "select replace(replace('{\"link\":\"x\"}', '{\"link\":\"', ''), '\"}', '')"
        )?,
        text("x")
    );
    assert_eq!(
        scalar(&db, "select replace('héllo wörld', 'ö', 'o')")?,
        text("héllo world")
    );
    Ok(())
}

#[test]
fn replace_on_bounded_columns_in_dml() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;
    db.ddl(
        "create table r (id int primary key, v varchar(20) null, c char(5) null, t longtext null)",
    )?;
    db.run("insert into r values (1, 'a.b', 'x.y', 'p.q'), (2, 'c.d', 'z', null)")?
        .done()?;

    assert_eq!(
        rows(
            &db,
            "select replace(v, '.', '/'), replace(c, '.', '/') from r order by id"
        )?,
        vec![vec![text("a/b"), text("x/y")], vec![text("c/d"), text("z")]]
    );
    db.run("update r set v = replace(v, '.', '_') where replace(t, '.', '') = 'pq'")?
        .done()?;
    db.run("insert into r values (3, replace('e.f', '.', '-'), null, null)")?
        .done()?;
    db.run("delete from r where replace(v, '.', '') = 'cd'")?
        .done()?;
    assert_eq!(
        rows(&db, "select id, v from r order by id")?,
        vec![
            vec![DataValue::Int32(1), text("a_b")],
            vec![DataValue::Int32(3), text("e-f")]
        ]
    );

    let mut session = db.session();
    session.run("BEGIN")?.done()?;
    session
        .run("update r set v = replace(v, '_', '+') where id = 1")?
        .done()?;
    session.run("COMMIT")?.done()?;
    drop(session);
    assert_eq!(scalar(&db, "select v from r where id = 1")?, text("a+b"));
    Ok(())
}

#[test]
fn preside_link_backfill_statement() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;
    db.ddl(
        "create table `psys_email_template_send_log_activity` ( `id` varchar(35) not null primary key, \
         `link` varchar(2000) null, `extra_data` longtext null )",
    )?;
    db.run(
        "insert into `psys_email_template_send_log_activity` values \
         ('a', null, '{\"link\":\"https://example.com/x?y=1\"}'), \
         ('b', 'keep', '{}'), \
         ('c', 'keep', null), \
         ('d', null, '{\"link\":\"\"}')",
    )?
    .done()?;

    db.run(
        "update `psys_email_template_send_log_activity`
           set `link` = Replace( Replace( `extra_data`, '{\"link\":\"', '' ), '\"}', '' )
           where `extra_data` is not null
           and `extra_data` != '{}'",
    )?
    .done()?;

    assert_eq!(
        rows(
            &db,
            "select id, link from psys_email_template_send_log_activity order by id"
        )?,
        vec![
            vec![text("a"), text("https://example.com/x?y=1")],
            vec![text("b"), text("keep")],
            vec![text("c"), text("keep")],
            vec![text("d"), text("")],
        ]
    );
    Ok(())
}

#[test]
fn length_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select length('hello')")?, DataValue::Int32(5));
    assert_eq!(scalar(&db, "select length('héllo')")?, DataValue::Int32(6));
    assert_eq!(scalar(&db, "select length('')")?, DataValue::Int32(0));
    assert_eq!(scalar(&db, "select length(null)")?, DataValue::Null);
    Ok(())
}

#[test]
fn length_in_order_by_on_varchar_column() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;
    db.ddl("create table domains (id int primary key, domain varchar(255))")?;
    db.run(
        "insert into domains values (1, 'example.com'), (2, 'x.co'), (3, 'verylongdomainname.org')",
    )?
    .done()?;

    assert_eq!(
        rows(
            &db,
            "select domain from domains order by Length(domain) desc"
        )?,
        vec![
            vec![text("verylongdomainname.org")],
            vec![text("example.com")],
            vec![text("x.co")]
        ]
    );
    Ok(())
}

#[test]
fn concat_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select concat('a', 'b')")?, text("ab"));
    assert_eq!(scalar(&db, "select concat('a', 'b', 'c')")?, text("abc"));
    assert_eq!(
        scalar(&db, "select concat('a', null, 'c')")?,
        DataValue::Null
    );

    assert_eq!(scalar(&db, "select concat_ws(',', 'a', 'b')")?, text("a,b"));
    assert_eq!(
        scalar(&db, "select concat_ws('-', 'a', 'b', 'c')")?,
        text("a-b-c")
    );
    assert_eq!(
        scalar(&db, "select concat_ws(',', 'a', null, 'c')")?,
        text("a,c")
    );
    assert_eq!(
        scalar(&db, "select concat_ws(null, 'a', 'b')")?,
        DataValue::Null
    );
    Ok(())
}

#[test]
fn substring_index_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select substring_index('a.b.c.d', '.', 2)")?,
        text("a.b")
    );
    assert_eq!(
        scalar(&db, "select substring_index('a.b.c.d', '.', -2)")?,
        text("c.d")
    );
    assert_eq!(
        scalar(&db, "select substring_index('a.b', '.', 0)")?,
        text("")
    );
    assert_eq!(
        scalar(&db, "select substring_index('test', '.', 5)")?,
        text("test")
    );
    assert_eq!(
        scalar(&db, "select substring_index(null, '.', 1)")?,
        DataValue::Null
    );
    Ok(())
}

#[test]
fn locate_instr_position_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(
        scalar(&db, "select locate('bar', 'foobarbar')")?,
        DataValue::Int32(4)
    );
    assert_eq!(
        scalar(&db, "select locate('bar', 'foobarbar', 5)")?,
        DataValue::Int32(7)
    );
    assert_eq!(
        scalar(&db, "select locate('xyz', 'foobar')")?,
        DataValue::Int32(0)
    );
    assert_eq!(scalar(&db, "select locate(null, 'test')")?, DataValue::Null);

    assert_eq!(
        scalar(&db, "select instr('foobarbar', 'bar')")?,
        DataValue::Int32(4)
    );
    assert_eq!(
        scalar(&db, "select instr('foobar', 'xyz')")?,
        DataValue::Int32(0)
    );

    assert_eq!(
        scalar(&db, "select position('bar', 'foobarbar')")?,
        DataValue::Int32(4)
    );
    Ok(())
}

#[test]
fn left_right_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select left('foobar', 3)")?, text("foo"));
    assert_eq!(scalar(&db, "select right('foobar', 3)")?, text("bar"));
    assert_eq!(scalar(&db, "select left('test', 10)")?, text("test"));
    assert_eq!(scalar(&db, "select left(null, 3)")?, DataValue::Null);
    Ok(())
}

#[test]
fn lpad_rpad_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select lpad('hi', 5, '?')")?, text("???hi"));
    assert_eq!(scalar(&db, "select rpad('hi', 5, '?')")?, text("hi???"));
    assert_eq!(scalar(&db, "select lpad('hello', 3, 'x')")?, text("hel"));
    assert_eq!(scalar(&db, "select lpad('hi', 4, 'ab')")?, text("abhi"));
    assert_eq!(scalar(&db, "select lpad('test', 5, '')")?, DataValue::Null);
    Ok(())
}

#[test]
fn repeat_reverse_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select repeat('ha', 3)")?, text("hahaha"));
    assert_eq!(scalar(&db, "select repeat('test', 0)")?, text(""));
    assert_eq!(scalar(&db, "select repeat(null, 3)")?, DataValue::Null);

    assert_eq!(scalar(&db, "select reverse('hello')")?, text("olleh"));
    assert_eq!(scalar(&db, "select reverse(null)")?, DataValue::Null);
    Ok(())
}

#[test]
fn ltrim_rtrim_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select ltrim('  hello')")?, text("hello"));
    assert_eq!(scalar(&db, "select rtrim('hello  ')")?, text("hello"));
    assert_eq!(scalar(&db, "select ltrim(null)")?, DataValue::Null);
    Ok(())
}

#[test]
fn hex_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select hex('abc')")?, text("616263"));
    assert_eq!(scalar(&db, "select hex('A')")?, text("41"));
    assert_eq!(scalar(&db, "select hex('')")?, text(""));
    assert_eq!(scalar(&db, "select hex(null)")?, DataValue::Null);
    Ok(())
}

#[test]
fn uuid_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    let uuid1 = scalar(&db, "select uuid()")?;
    let uuid2 = scalar(&db, "select uuid()")?;
    assert_ne!(uuid1, uuid2);
    assert!(matches!(uuid1, DataValue::Utf8 { .. }));
    Ok(())
}

#[test]
fn greatest_least_functions() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select greatest('a', 'b', 'c')")?, text("c"));
    assert_eq!(scalar(&db, "select greatest('z', 'a')")?, text("z"));
    assert_eq!(
        scalar(&db, "select greatest('a', null, 'c')")?,
        DataValue::Null
    );

    assert_eq!(scalar(&db, "select least('a', 'b', 'c')")?, text("a"));
    assert_eq!(scalar(&db, "select least('z', 'a')")?, text("a"));
    assert_eq!(
        scalar(&db, "select least('a', null, 'c')")?,
        DataValue::Null
    );
    Ok(())
}

#[test]
fn ifnull_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = database(&dir)?;

    assert_eq!(scalar(&db, "select ifnull('a', 'b')")?, text("a"));
    assert_eq!(scalar(&db, "select ifnull(null, 'b')")?, text("b"));
    assert_eq!(scalar(&db, "select ifnull(null, null)")?, DataValue::Null);
    Ok(())
}

#[test]
fn round_function() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = database(&dir)?;

    db.ddl("create table t (id int primary key, val double)")?;
    db.run("insert into t values (1, 1.5), (2, 1.234), (3, 1.567)")?
        .done()?;

    use ordered_float::OrderedFloat;
    assert_eq!(
        scalar(&db, "select round(val) from t where id = 1")?,
        DataValue::Float64(OrderedFloat(2.0))
    );
    assert_eq!(
        scalar(&db, "select round(val, 2) from t where id = 2")?,
        DataValue::Float64(OrderedFloat(1.23))
    );
    assert_eq!(
        scalar(&db, "select round(val, 1) from t where id = 3")?,
        DataValue::Float64(OrderedFloat(1.6))
    );
    assert_eq!(
        scalar(&db, "select round(cast(null as double))")?,
        DataValue::Null
    );
    Ok(())
}
