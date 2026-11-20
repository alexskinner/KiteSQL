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

//! The statement diagnostics channel: a dialect's policy and code mapping, end to end.

#![cfg(any(feature = "lmdb", feature = "slatedb"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
use kite_sql::db::DataBaseBuilder;
use kite_sql::diagnostics::{
    Diagnostic, DiagnosticKind, DiagnosticLevel, DivisionByZero, StatementPolicy,
};
use kite_sql::dialect::SqlDialect;
use kite_sql::errors::DatabaseError;
use kite_sql::types::value::DataValue;
use kite_sql_serde_macros::maybe_async;
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

#[maybe_async]
async fn scalar<S: kite_sql::storage::Storage>(
    db: &kite_sql::db::Database<S>,
    sql: &str,
) -> Result<DataValue, DatabaseError> {
    let mut iter = db.run(sql).await?;
    let row = iter
        .next_tuple(|_, t| t[0].clone())
        .await?
        .expect("one row");
    iter.done().await?;
    Ok(row)
}

#[maybe_async]
async fn exec<S: kite_sql::storage::Storage>(
    db: &kite_sql::db::Database<S>,
    sql: &str,
) -> Result<(), DatabaseError> {
    db.run(sql).await?.done().await
}

fn codes<S: kite_sql::storage::Storage>(
    db: &kite_sql::db::Database<S>,
) -> Vec<(DiagnosticLevel, u32, String)> {
    db.take_diagnostics()
        .into_iter()
        .map(|d| (d.level, d.code, d.message))
        .collect()
}

#[maybe_async]
#[test]
async fn dialect_policy_and_mapping() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path())
        .register_dialect(Arc::new(LenientDialect {
            parser: GenericDialect {},
        }))
        .default_dialect("lenient")?
        .build_test_db()
        .await?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY, d DECIMAL(5, 2), n INT)")
        .await?;

    assert_eq!(scalar(&db, "SELECT 1 / 0").await?, DataValue::Null);
    assert_eq!(
        codes(&db),
        vec![(DiagnosticLevel::Warning, 9001, "division by zero".into())]
    );
    assert_eq!(scalar(&db, "SELECT 7 % 0").await?, DataValue::Null);
    assert_eq!(codes(&db).len(), 1);
    assert_eq!(
        scalar(&db, "SELECT CAST('12abc' AS BIGINT)").await?,
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
        scalar(&db, "SELECT CAST(' 5 ' AS BIGINT)").await?,
        DataValue::Int64(5)
    );
    assert!(codes(&db).is_empty());

    // writes keep the errors
    assert!(exec(&db, "INSERT INTO t VALUES (1, 1, 1 / 0)")
        .await
        .is_err());
    assert!(exec(&db, "INSERT INTO t VALUES (1, 1, '12abc')")
        .await
        .is_err());

    // storing into DECIMAL(5, 2) rounds away digits: a Note per affected row
    db.run("INSERT INTO t VALUES (1, 1.234, 0), (2, 1.5, 0), (3, '2.001', 0)")
        .await?
        .done()
        .await?;
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

#[maybe_async]
#[test]
async fn generic_dialect_keeps_errors() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_test_db().await?;
    assert!(exec(&db, "SELECT 1 / 0").await.is_err());
    assert!(
        exec(&db, "SELECT CAST(1.0 AS DECIMAL) / CAST(0 AS DECIMAL)")
            .await
            .is_err()
    );
    assert!(exec(&db, "SELECT CAST('12abc' AS BIGINT)").await.is_err());
    assert!(db.take_diagnostics().is_empty());
    Ok(())
}
