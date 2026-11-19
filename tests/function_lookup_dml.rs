// Repro: functions registered via a FunctionPack / register_scalar_function
// must be resolvable in every statement kind, not only SELECT.
#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;
use kite_sql::expression::function::scala::ScalarFunctionImpl;
use kite_sql::expression::function::{FunctionSummary, PersistencePolicy};
use kite_sql::types::value::DataValue;
use kite_sql::types::LogicalType;
use kite_sql::{FunctionPack, FunctionRegistry};
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Debug)]
struct Pack;
impl FunctionPack for Pack {
    fn name(&self) -> &str {
        "lookup_pack"
    }
    fn install(&self, registry: &mut FunctionRegistry) -> Result<(), DatabaseError> {
        registry.register_scalar(Times10::new())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Times10 {
    summary: FunctionSummary,
    return_type: LogicalType,
}
impl Times10 {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "times10".into(),
                arg_types: vec![LogicalType::Integer],
                persistence: PersistencePolicy::Transient,
                accepts_any_args: false,
            },
            return_type: LogicalType::Integer,
        })
    }
}
impl ScalarFunctionImpl for Times10 {
    fn eval(
        &self,
        args: &[kite_sql::planner::ExprRef],
        arena: &(dyn kite_sql::planner::MetaArena + '_),
        tuple: Option<&dyn kite_sql::types::tuple::Input>,
    ) -> Result<DataValue, DatabaseError> {
        let v = arena.expression(args[0]).eval(arena, tuple)?;
        Ok(match v.as_ref() {
            DataValue::Int32(x) => DataValue::Int32(x * 10),
            _ => v.into_owned(),
        })
    }
    fn return_type(&self) -> &LogicalType {
        &self.return_type
    }
    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }
    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

macro_rules! rows {
    ($runner:expr, $sql:expr) => {{
        let mut iter = $runner.run($sql)?;
        let mut out = Vec::new();
        while let Some(r) = iter.next_tuple(|_, t| t.take_values())? {
            out.push(r);
        }
        iter.done()?;
        out
    }};
}

fn check(label: &str, pack: bool) -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let b = DataBaseBuilder::path(dir.path());
    let b = if pack {
        b.with_function_pack(Arc::new(Pack))
    } else {
        b.register_scalar_function(Times10::new())
    };
    let mut db = b.build_lmdb()?;
    db.ddl("create table t (id int primary key, c int)")?;
    db.run("insert into t values (1, 1), (2, 2)")?.done()?;
    eprintln!("[{label}] select");
    assert_eq!(
        rows!(db, "select times10(c) from t order by id"),
        vec![vec![DataValue::Int32(10)], vec![DataValue::Int32(20)]]
    );
    eprintln!("[{label}] update");
    db.run("update t set c = times10(c)")?.done()?;
    assert_eq!(
        rows!(db, "select c from t order by id"),
        vec![vec![DataValue::Int32(10)], vec![DataValue::Int32(20)]]
    );
    eprintln!("[{label}] delete");
    db.run("delete from t where times10(c) = 200")?.done()?;
    assert_eq!(
        rows!(db, "select id from t order by id"),
        vec![vec![DataValue::Int32(1)]]
    );
    eprintln!("[{label}] insert");
    db.run("insert into t values (3, times10(3))")?.done()?;
    assert_eq!(
        rows!(db, "select c from t where id = 3"),
        vec![vec![DataValue::Int32(30)]]
    );
    eprintln!("[{label}] session update/delete/insert");
    let mut s = db.session();
    s.run("update t set c = times10(c) where id = 3")?.done()?;
    s.run("delete from t where times10(id) = 10")?.done()?;
    s.run("insert into t values (4, times10(4))")?.done()?;
    s.run("BEGIN")?.done()?;
    s.run("update t set c = times10(c) where id = 4")?.done()?;
    s.run("COMMIT")?.done()?;
    drop(s);
    assert_eq!(
        rows!(db, "select id, c from t order by id"),
        vec![
            vec![DataValue::Int32(3), DataValue::Int32(300)],
            vec![DataValue::Int32(4), DataValue::Int32(400)]
        ]
    );
    Ok(())
}

#[test]
fn pack_function_in_dml() -> Result<(), DatabaseError> {
    check("pack", true)
}

#[test]
fn builder_udf_in_dml() -> Result<(), DatabaseError> {
    check("udf", false)
}

// A UDF declared over unbounded VARCHAR must accept bounded VARCHAR(n) / CHAR(n) columns.
#[derive(Debug, Clone)]
struct Exclaim {
    summary: FunctionSummary,
    return_type: LogicalType,
}
impl Exclaim {
    fn new() -> Arc<Self> {
        let s = LogicalType::Varchar(None, kite_sql::types::CharLengthUnits::Characters);
        Arc::new(Self {
            summary: FunctionSummary {
                name: "exclaim".into(),
                arg_types: vec![s.clone()],
                persistence: PersistencePolicy::Transient,
                accepts_any_args: false,
            },
            return_type: s,
        })
    }
}
impl ScalarFunctionImpl for Exclaim {
    fn eval(
        &self,
        args: &[kite_sql::planner::ExprRef],
        arena: &(dyn kite_sql::planner::MetaArena + '_),
        tuple: Option<&dyn kite_sql::types::tuple::Input>,
    ) -> Result<DataValue, DatabaseError> {
        let v = arena.expression(args[0]).eval(arena, tuple)?;
        Ok(match v.as_ref() {
            DataValue::Null => DataValue::Null,
            other => DataValue::from(format!("{other}!")),
        })
    }
    fn return_type(&self) -> &LogicalType {
        &self.return_type
    }
    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }
    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

#[test]
fn unbounded_varchar_udf_accepts_bounded_columns() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path())
        .register_scalar_function(Exclaim::new())
        .build_lmdb()?;
    db.ddl("create table s (id int primary key, v varchar(20), c char(3), l varchar)")?;
    db.run("insert into s values (1, 'a', 'b', 'c')")?.done()?;
    let s = |x: &str| DataValue::from(x.to_string());
    assert_eq!(rows!(db, "select exclaim('lit')"), vec![vec![s("lit!")]]);
    assert_eq!(rows!(db, "select exclaim(l) from s"), vec![vec![s("c!")]]);
    assert_eq!(
        rows!(db, "select exclaim(v), exclaim(c) from s"),
        vec![vec![s("a!"), s("b!")]]
    );
    db.run("update s set v = exclaim(v), l = exclaim(c) where exclaim(v) = 'a!'")?
        .done()?;
    assert_eq!(
        rows!(db, "select v, l from s"),
        vec![vec![s("a!"), s("b!")]]
    );
    db.run("delete from s where exclaim(v) = 'a!!'")?.done()?;
    assert_eq!(
        rows!(db, "select count(*) from s"),
        vec![vec![DataValue::Int32(0)]]
    );
    Ok(())
}

#[test]
fn wrong_argument_type_reports_signature() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path())
        .register_scalar_function(Exclaim::new())
        .build_lmdb()?;
    let err = db
        .run("select exclaim(1.5)")
        .err()
        .expect("expected an error")
        .to_string();
    assert!(err.contains("argument types"), "unexpected error: {err}");
    Ok(())
}
