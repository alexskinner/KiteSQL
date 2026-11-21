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

#![cfg(all(not(feature = "async"), feature = "lmdb"))]

//! The statement diagnostics channel: a dialect's policy and code mapping, end to end.

use kite_sql::db::DataBaseBuilder;
use kite_sql::diagnostics::{
    Diagnostic, DiagnosticKind, DiagnosticLevel, DivisionByZero, StatementPolicy,
};
use kite_sql::dialect::SqlDialect;
use kite_sql::errors::DatabaseError;
use kite_sql::types::value::DataValue;
use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect};
use std::sync::Arc;
use tempfile::TempDir;

/// Queries are lenient (NULL for x/0, numeric string prefixes); writes stay strict.
/// Every diagnostic gets code 9000 + a per-kind offset.
#[derive(Debug)]
struct LenientDialect {
    parser: GenericDialect,
}

impl SqlDialect for LenientDialect {
    fn name(&self) -> &str {
        "lenient"
    }

    fn parser_dialect(&self) -> &dyn SqlParserDialect {
        &self.parser
    }

    fn statement_policy(&self, stmt: &Statement) -> StatementPolicy {
        match stmt {
            Statement::Query(_) => StatementPolicy {
                division_by_zero: DivisionByZero::NullWithWarning,
                lenient_numeric_strings: true,
            },
            _ => StatementPolicy::default(),
        }
    }

    fn map_diagnostic(&self, mut d: Diagnostic) -> Option<Diagnostic> {
        d.code = match d.kind {
            DiagnosticKind::DivisionByZero => 9001,
            DiagnosticKind::TruncatedNumber { .. } => 9002,
            DiagnosticKind::DataTruncated { .. } => 9003,
            _ => return None,
        };
        Some(d)
    }
}

fn scalar<S: kite_sql::storage::Storage>(
    db: &kite_sql::db::Database<S>,
    sql: &str,
) -> Result<DataValue, DatabaseError> {
    let mut iter = db.run(sql)?;
    let row = iter.next_tuple(|_, t| t[0].clone())?.expect("one row");
    iter.done()?;
    Ok(row)
}

fn codes<S: kite_sql::storage::Storage>(
    db: &kite_sql::db::Database<S>,
) -> Vec<(DiagnosticLevel, u32, String)> {
    db.take_diagnostics()
        .into_iter()
        .map(|d| (d.level, d.code, d.message))
        .collect()
}

#[test]
fn dialect_policy_and_mapping() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path())
        .register_dialect(Arc::new(LenientDialect {
            parser: GenericDialect {},
        }))
        .default_dialect("lenient")?
        .build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, d DECIMAL(5, 2), n INT)")?;

    assert_eq!(scalar(&db, "SELECT 1 / 0")?, DataValue::Null);
    assert_eq!(
        codes(&db),
        vec![(DiagnosticLevel::Warning, 9001, "division by zero".into())]
    );
    assert_eq!(scalar(&db, "SELECT 7 % 0")?, DataValue::Null);
    assert_eq!(codes(&db).len(), 1);
    assert_eq!(
        scalar(&db, "SELECT CAST('12abc' AS BIGINT)")?,
        DataValue::Int64(12)
    );
    assert_eq!(
        codes(&db),
        vec![(
            DiagnosticLevel::Warning,
            9002,
            "truncated incorrect INTEGER value: '12abc'".into()
        )]
    );
    // a clean statement starts with an empty diagnostics list
    assert_eq!(
        scalar(&db, "SELECT CAST(' 5 ' AS BIGINT)")?,
        DataValue::Int64(5)
    );
    assert!(codes(&db).is_empty());

    // writes keep the errors
    assert!(db
        .run("INSERT INTO t VALUES (1, 1, 1 / 0)")
        .and_then(|i| i.done())
        .is_err());
    assert!(db
        .run("INSERT INTO t VALUES (1, 1, '12abc')")
        .and_then(|i| i.done())
        .is_err());

    // storing into DECIMAL(5, 2) rounds away digits: a Note per affected row
    db.run("INSERT INTO t VALUES (1, 1.234, 0), (2, 1.5, 0), (3, '2.001', 0)")?
        .done()?;
    assert_eq!(
        codes(&db),
        vec![
            (
                DiagnosticLevel::Note,
                9003,
                "data truncated for column 'd' at row 1".into()
            ),
            (
                DiagnosticLevel::Note,
                9003,
                "data truncated for column 'd' at row 3".into()
            ),
        ]
    );
    Ok(())
}

#[test]
fn generic_dialect_keeps_errors() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    assert!(db.run("SELECT 1 / 0").and_then(|i| i.done()).is_err());
    assert!(db
        .run("SELECT CAST(1.0 AS DECIMAL) / CAST(0 AS DECIMAL)")
        .and_then(|i| i.done())
        .is_err());
    assert!(db
        .run("SELECT CAST('12abc' AS BIGINT)")
        .and_then(|i| i.done())
        .is_err());
    assert!(db.take_diagnostics().is_empty());
    Ok(())
}
