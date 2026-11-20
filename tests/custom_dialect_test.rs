// Copyright 2024 KipData/KiteSQL
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Test that custom dialects defined in external crates can map types and change parsing.

#![cfg(any(feature = "lmdb", feature = "slatedb"))]

mod common;
// `#[maybe_async]` tests run on `crate::maybe_async::block_on`.
#[allow(unused_imports)]
use common as maybe_async;
#[cfg(any(feature = "lmdb", feature = "slatedb"))]
#[allow(unused_imports)]
use common::TestDatabaseBuilder as _;
use kite_sql::db::DataBaseBuilder;
use kite_sql::dialect::SqlDialect;
use kite_sql::errors::DatabaseError;
use kite_sql::expression::BinaryOperator;
use kite_sql::types::value::DataValue;
use kite_sql::types::{CharLengthUnits, LogicalType};
use kite_sql_serde_macros::maybe_async;
use sqlparser::ast::{DataType, Ident, ObjectName, Statement};
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect};
use std::sync::Arc;

/// A toy custom sqlparser dialect that supports backticks like MySQL.
#[derive(Debug)]
struct ToyParserDialect;

impl SqlParserDialect for ToyParserDialect {
    fn is_identifier_start(&self, ch: char) -> bool {
        GenericDialect {}.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        GenericDialect {}.is_identifier_part(ch)
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        // Unlike GenericDialect, also support backticks
        ch == '`' || ch == '"'
    }

    fn supports_filter_during_aggregation(&self) -> bool {
        true
    }
}

/// A toy custom dialect for testing that maps "CUSTOMTEXT" to Varchar and changes parsing.
#[derive(Debug)]
struct ToyDialect {
    parser: ToyParserDialect,
}

impl ToyDialect {
    fn new() -> Self {
        Self {
            parser: ToyParserDialect,
        }
    }
}

impl SqlDialect for ToyDialect {
    fn name(&self) -> &str {
        "toy"
    }

    fn parser_dialect(&self) -> &dyn SqlParserDialect {
        &self.parser
    }

    fn map_data_type(&self, data_type: &DataType) -> Option<Result<LogicalType, DatabaseError>> {
        match data_type {
            DataType::Custom(name, _) if name.0.len() == 1 => {
                if let sqlparser::ast::ObjectNamePart::Identifier(ident) = &name.0[0] {
                    if ident.value.to_lowercase() == "customtext" {
                        return Some(Ok(LogicalType::Varchar(None, CharLengthUnits::Characters)));
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// AVG of an INT is a DECIMAL with two decimals in this dialect.
    fn avg_return_type(&self, arg: &LogicalType) -> Option<LogicalType> {
        matches!(arg, LogicalType::Integer).then_some(LogicalType::Decimal(None, Some(2)))
    }

    /// Numbers with a decimal point are exact DECIMALs in this dialect.
    #[cfg(feature = "decimal")]
    fn number_literal(
        &self,
        text: &str,
    ) -> Option<Result<(DataValue, LogicalType), DatabaseError>> {
        if !text.contains('.') || text.contains(['e', 'E']) {
            return None;
        }
        let value: rust_decimal::Decimal = text.parse().ok()?;
        Some(Ok((
            DataValue::Decimal(value),
            LogicalType::Decimal(None, Some(value.scale() as u8)),
        )))
    }

    /// `/` and `*` on INT or DECIMAL operands give a DECIMAL with two decimals in this dialect.
    fn arithmetic_return_type(
        &self,
        op: &BinaryOperator,
        left: &LogicalType,
        right: &LogicalType,
    ) -> Option<LogicalType> {
        let exact =
            |ty: &LogicalType| matches!(ty, LogicalType::Integer | LogicalType::Decimal(..));
        (matches!(op, BinaryOperator::Divide | BinaryOperator::Multiply)
            && exact(left)
            && exact(right))
        .then_some(LogicalType::Decimal(None, Some(2)))
    }

    /// NULLs sort as the lowest value in this dialect: first for ASC, last for DESC.
    fn nulls_first(&self, asc: bool) -> bool {
        asc
    }

    fn rewrite(&self, statement: &mut Statement) -> Result<(), DatabaseError> {
        // Rewrite table names that are exactly "customtable" to "rewritten_table"
        match statement {
            Statement::CreateTable(create) => {
                if let Some(sqlparser::ast::ObjectNamePart::Identifier(ident)) =
                    create.name.0.first()
                {
                    if ident.value.eq_ignore_ascii_case("customtable") {
                        create.name = ObjectName(vec![sqlparser::ast::ObjectNamePart::Identifier(
                            Ident::new("rewritten_table"),
                        )]);
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[maybe_async]
#[test]
#[cfg(feature = "parser")]
async fn test_custom_dialect_type_mapping() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = tempfile::tempdir()?;
    let toy_dialect = Arc::new(ToyDialect::new());
    let storage = common::test_storage(temp_dir.path()).await?;
    let mut database = DataBaseBuilder::path(".")
        .register_dialect(toy_dialect)
        .default_dialect("toy")?
        .build_with_storage(storage)?;

    // Verify custom type mapping works (CUSTOMTEXT should map to VARCHAR)
    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, text CUSTOMTEXT)")
        .await?;

    // If we got here, the custom type was successfully mapped
    // This proves that external dialects can map types through the SqlDialect trait

    Ok(())
}

#[maybe_async]
#[test]
#[cfg(feature = "parser")]
async fn test_custom_dialect_parsing_and_rewrite() -> Result<(), Box<dyn std::error::Error>> {
    use kite_sql::parser::parse_sql_with_dialect;
    let temp_dir = tempfile::tempdir()?;
    let toy_dialect = Arc::new(ToyDialect::new());
    let storage = common::test_storage(temp_dir.path()).await?;
    let mut database = DataBaseBuilder::path(".")
        .register_dialect(toy_dialect.clone())
        .default_dialect("toy")?
        .build_with_storage(storage)?;

    // Demonstrate parsing works with custom ToyParserDialect
    // ToyParserDialect supports backticks (is_delimited_identifier_start returns true for '`')
    let toy_parse = parse_sql_with_dialect(
        "CREATE TABLE `backtick_table` (id INT)",
        toy_dialect.parser_dialect(),
    );
    assert!(
        toy_parse.is_ok(),
        "ToyParserDialect should successfully parse backtick identifiers"
    );

    // Verify the parsing produces a valid statement
    let statements = toy_parse.unwrap();
    assert_eq!(statements.len(), 1, "Should parse exactly one statement");

    // Compare: Generic dialect behavior is the same for backticks (both support it)
    // but the custom dialect can implement other parsing differences as needed.
    // The key demonstration here is that we can use a custom parser dialect.

    // Demonstrate the rewrite hook: CUSTOMTABLE gets rewritten to rewritten_table
    // The CREATE TABLE statement with "CUSTOMTABLE" will be rewritten to "rewritten_table"
    database
        .ddl("CREATE TABLE CUSTOMTABLE (val INT PRIMARY KEY)")
        .await?;

    // The rewrite hook should have transformed CUSTOMTABLE -> rewritten_table
    // So we can insert and query using the rewritten name
    database
        .run("INSERT INTO rewritten_table VALUES (42)")
        .await?
        .done()
        .await?;

    let mut result = database.run("SELECT val FROM rewritten_table").await?;
    let row = result.next_tuple(|_, row| row.clone()).await?.unwrap();
    assert_eq!(row[0], kite_sql::types::value::DataValue::Int32(42));
    result.done().await?;

    // Verify that trying to use "CUSTOMTABLE" directly fails (it was rewritten)
    let result = database.run("SELECT val FROM CUSTOMTABLE").await;
    assert!(
        result.is_err(),
        "CUSTOMTABLE should not exist; it was rewritten to rewritten_table"
    );

    // This proves that external dialects can:
    // 1. Rewrite AST before execution (CUSTOMTABLE -> rewritten_table)
    // 2. Map custom types (CUSTOMTEXT -> VARCHAR from type mapping test)
    // 3. Change parsing behavior (ToyParserDialect supports backtick identifiers)

    Ok(())
}

#[maybe_async]
#[test]
#[cfg(feature = "parser")]
async fn test_generic_dialect_unchanged() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = tempfile::tempdir()?;
    let storage = common::test_storage(temp_dir.path()).await?;
    let mut database = kite_sql::db::DataBaseBuilder::path(".").build_with_storage(storage)?;

    database
        .ddl("CREATE TABLE test (id INT PRIMARY KEY, name VARCHAR(50))")
        .await?;

    let mut result = database.run("DESCRIBE test").await?;

    let id_row = result.next_tuple(|_, row| row.clone()).await?.unwrap();
    assert_eq!(
        id_row[0],
        kite_sql::types::value::DataValue::from("id".to_string())
    );
    assert_eq!(
        id_row[1],
        kite_sql::types::value::DataValue::from("Integer".to_string())
    );

    let name_row = result.next_tuple(|_, row| row.clone()).await?.unwrap();
    assert_eq!(
        name_row[0],
        kite_sql::types::value::DataValue::from("name".to_string())
    );
    assert_eq!(
        name_row[1],
        kite_sql::types::value::DataValue::from("Varchar(Some(50), CHARACTERS)".to_string())
    );

    result.done().await?;

    Ok(())
}

#[maybe_async]
#[test]
#[cfg(feature = "parser")]
async fn test_custom_dialect_avg_return_type() -> Result<(), Box<dyn std::error::Error>> {
    async fn avg(
        database: &kite_sql::db::Database<common::TestStorage>,
        sql: &str,
    ) -> Result<(LogicalType, DataValue), DatabaseError> {
        let mut result = database.run(sql).await?;
        let ty = result.schema(|schema| schema.get(0).unwrap().datatype().clone());
        let row = result.next_tuple(|_, row| row[0].clone()).await?.unwrap();
        result.done().await?;
        Ok((ty, row))
    }
    let setup = "CREATE TABLE a (id INT PRIMARY KEY, n INT, f DOUBLE)";
    let rows = "INSERT INTO a VALUES (1, 1, 1.0), (2, 2, 2.0), (3, 4, 4.0), (4, NULL, NULL)";

    // The dialect types AVG(INT) DECIMAL(_, 2): exact, rounded half away from zero.
    let temp_dir = tempfile::tempdir()?;
    let mut toy = DataBaseBuilder::path(".")
        .register_dialect(Arc::new(ToyDialect::new()))
        .default_dialect("toy")?
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    toy.ddl(setup).await?;
    toy.run(rows).await?.done().await?;
    let (ty, value) = avg(&toy, "SELECT AVG(n) FROM a").await?;
    assert_eq!(ty, LogicalType::Decimal(None, Some(2)));
    assert_eq!(value.to_string(), "2.33");
    // Arguments the dialect leaves alone keep DOUBLE.
    let (ty, value) = avg(&toy, "SELECT AVG(f) FROM a").await?;
    assert_eq!(ty, LogicalType::Double);
    assert!(matches!(value, DataValue::Float64(_)));

    // The generic dialect keeps AVG DOUBLE.
    let temp_dir = tempfile::tempdir()?;
    let mut generic = DataBaseBuilder::path(".")
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    generic.ddl(setup).await?;
    generic.run(rows).await?.done().await?;
    let (ty, value) = avg(&generic, "SELECT AVG(n) FROM a").await?;
    assert_eq!(ty, LogicalType::Double);
    assert!(matches!(value, DataValue::Float64(_)));
    Ok(())
}

#[maybe_async]
#[test]
#[cfg(all(feature = "parser", feature = "decimal"))]
async fn test_custom_dialect_number_literal_and_arithmetic_type(
) -> Result<(), Box<dyn std::error::Error>> {
    async fn first(
        database: &kite_sql::db::Database<common::TestStorage>,
        sql: &str,
    ) -> Result<(LogicalType, DataValue), DatabaseError> {
        let mut result = database.run(sql).await?;
        let ty = result.schema(|schema| schema.get(0).unwrap().datatype().clone());
        let row = result.next_tuple(|_, row| row[0].clone()).await?.unwrap();
        result.done().await?;
        Ok((ty, row))
    }
    let setup = "CREATE TABLE a (id INT PRIMARY KEY, n INT)";
    let rows = "INSERT INTO a VALUES (1, 7), (2, 1)";

    let temp_dir = tempfile::tempdir()?;
    let mut toy = DataBaseBuilder::path(".")
        .register_dialect(Arc::new(ToyDialect::new()))
        .default_dialect("toy")?
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    toy.ddl(setup).await?;
    toy.run(rows).await?.done().await?;
    // Decimal literals are exact, also negated.
    let (ty, value) = first(&toy, "SELECT 0.1 + 0.2").await?;
    assert!(matches!(ty, LogicalType::Decimal(..)));
    assert_eq!(value.to_string(), "0.3");
    let (_, value) = first(&toy, "SELECT -1.50").await?;
    assert_eq!(value.to_string(), "-1.50");
    // INT / INT is computed in DECIMAL and rounded half away from zero to the dialect scale.
    let (ty, value) = first(&toy, "SELECT n / 3 FROM a WHERE id = 1").await?;
    assert_eq!(ty, LogicalType::Decimal(None, Some(2)));
    assert_eq!(value.to_string(), "2.33");
    let (_, value) = first(&toy, "SELECT 2 / 3").await?;
    assert_eq!(value.to_string(), "0.67");
    // Chained arithmetic rounds once, at the end.
    let (_, value) = first(&toy, "SELECT n / 3 * 3 FROM a WHERE id = 2").await?;
    assert_eq!(value.to_string(), "1.00");
    // Literals with an exponent keep the engine's DOUBLE.
    let (ty, _) = first(&toy, "SELECT 1e2").await?;
    assert_eq!(ty, LogicalType::Double);

    // The generic dialect keeps DOUBLE literals and its own integer division.
    let temp_dir = tempfile::tempdir()?;
    let mut generic = DataBaseBuilder::path(".")
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    generic.ddl(setup).await?;
    generic.run(rows).await?.done().await?;
    let (ty, _) = first(&generic, "SELECT 0.1 + 0.2").await?;
    assert_eq!(ty, LogicalType::Double);
    let (ty, _) = first(&generic, "SELECT n / 3 FROM a WHERE id = 1").await?;
    assert!(!matches!(ty, LogicalType::Decimal(..)));
    Ok(())
}

#[maybe_async]
#[test]
#[cfg(feature = "parser")]
async fn test_custom_dialect_nulls_first() -> Result<(), Box<dyn std::error::Error>> {
    async fn column(
        database: &kite_sql::db::Database<common::TestStorage>,
        sql: &str,
    ) -> Result<Vec<String>, DatabaseError> {
        let mut result = database.run(sql).await?;
        let mut values = Vec::new();
        while let Some(value) = result.next_tuple(|_, row| row[0].to_string()).await? {
            values.push(value);
        }
        result.done().await?;
        Ok(values)
    }
    let setup = "CREATE TABLE s (id INT PRIMARY KEY, n INT)";
    let index = "CREATE INDEX s_n ON s (n)";
    let rows = "INSERT INTO s VALUES (1, 2), (2, NULL), (3, 1), (4, NULL)";
    let queries = [
        "SELECT n FROM s ORDER BY n, id",
        "SELECT n FROM s ORDER BY n DESC, id",
        "SELECT n FROM s ORDER BY n NULLS LAST, id",
        "SELECT n FROM s ORDER BY n DESC NULLS FIRST, id",
        "SELECT id FROM s ORDER BY row_number() OVER (ORDER BY n, id)",
        "SELECT n FROM s ORDER BY id DESC",
    ];

    let temp_dir = tempfile::tempdir()?;
    let mut toy = DataBaseBuilder::path(".")
        .register_dialect(Arc::new(ToyDialect::new()))
        .default_dialect("toy")?
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    toy.ddl(setup).await?;
    toy.ddl(index).await?;
    toy.run(rows).await?.done().await?;
    let mut toy_results = Vec::with_capacity(queries.len());
    for sql in queries.iter() {
        toy_results.push(column(&toy, sql).await?);
    }
    assert_eq!(
        toy_results,
        vec![
            vec!["NULL", "NULL", "1", "2"],
            vec!["2", "1", "NULL", "NULL"],
            vec!["1", "2", "NULL", "NULL"],
            vec!["NULL", "NULL", "2", "1"],
            vec!["2", "4", "3", "1"],
            vec!["NULL", "1", "NULL", "2"],
        ]
    );

    // The generic dialect keeps NULLs last in both directions.
    let temp_dir = tempfile::tempdir()?;
    let mut generic = DataBaseBuilder::path(".")
        .build_with_storage(common::test_storage(temp_dir.path()).await?)?;
    generic.ddl(setup).await?;
    generic.ddl(index).await?;
    generic.run(rows).await?.done().await?;
    let mut generic_results = Vec::with_capacity(queries.len());
    for sql in queries.iter() {
        generic_results.push(column(&generic, sql).await?);
    }
    assert_eq!(
        generic_results,
        vec![
            vec!["1", "2", "NULL", "NULL"],
            vec!["2", "1", "NULL", "NULL"],
            vec!["1", "2", "NULL", "NULL"],
            vec!["NULL", "NULL", "2", "1"],
            vec!["3", "1", "2", "4"],
            vec!["NULL", "1", "NULL", "2"],
        ]
    );
    Ok(())
}
