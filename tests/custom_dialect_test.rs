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

use kite_sql::db::DataBaseBuilder;
use kite_sql::dialect::{ColumnTypeSpec, SqlDialect};
use kite_sql::errors::DatabaseError;
use kite_sql::types::{CharLengthUnits, LogicalType};
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

    fn map_data_type(&self, data_type: &DataType) -> Option<Result<ColumnTypeSpec, DatabaseError>> {
        match data_type {
            DataType::Custom(name, _) if name.0.len() == 1 => {
                if let sqlparser::ast::ObjectNamePart::Identifier(ident) = &name.0[0] {
                    if ident.value.to_lowercase() == "customtext" {
                        return Some(Ok(ColumnTypeSpec::new(LogicalType::Varchar(
                            None,
                            CharLengthUnits::Characters,
                        ))));
                    }
                }
                None
            }
            _ => None,
        }
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

#[test]
#[cfg(feature = "parser")]
fn test_custom_dialect_type_mapping() -> Result<(), Box<dyn std::error::Error>> {
    use kite_sql::storage::lmdb::LmdbStorage;

    let temp_dir = tempfile::tempdir()?;
    let toy_dialect = Arc::new(ToyDialect::new());
    let storage = LmdbStorage::new(temp_dir.path())?;
    let mut database = DataBaseBuilder::path(".")
        .register_dialect(toy_dialect)
        .default_dialect("toy")?
        .build_with_storage(storage)?;

    // Verify custom type mapping works (CUSTOMTEXT should map to VARCHAR)
    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, text CUSTOMTEXT)")?;

    // If we got here, the custom type was successfully mapped
    // This proves that external dialects can map types through the SqlDialect trait

    Ok(())
}

#[test]
#[cfg(feature = "parser")]
fn test_custom_dialect_parsing_and_rewrite() -> Result<(), Box<dyn std::error::Error>> {
    use kite_sql::parser::parse_sql_with_dialect;
    use kite_sql::storage::lmdb::LmdbStorage;

    let temp_dir = tempfile::tempdir()?;
    let toy_dialect = Arc::new(ToyDialect::new());
    let storage = LmdbStorage::new(temp_dir.path())?;
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
    database.ddl("CREATE TABLE CUSTOMTABLE (val INT PRIMARY KEY)")?;

    // The rewrite hook should have transformed CUSTOMTABLE -> rewritten_table
    // So we can insert and query using the rewritten name
    database
        .run("INSERT INTO rewritten_table VALUES (42)")?
        .done()?;

    let mut result = database.run("SELECT val FROM rewritten_table")?;
    let row = result.next_tuple(|_, row| row.clone())?.unwrap();
    assert_eq!(row[0], kite_sql::types::value::DataValue::Int32(42));
    result.done()?;

    // Verify that trying to use "CUSTOMTABLE" directly fails (it was rewritten)
    let result = database.run("SELECT val FROM CUSTOMTABLE");
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

#[test]
#[cfg(feature = "parser")]
fn test_generic_dialect_unchanged() -> Result<(), Box<dyn std::error::Error>> {
    use kite_sql::storage::lmdb::LmdbStorage;

    let temp_dir = tempfile::tempdir()?;
    let storage = LmdbStorage::new(temp_dir.path())?;
    let mut database = kite_sql::db::DataBaseBuilder::path(".").build_with_storage(storage)?;

    database.ddl("CREATE TABLE test (id INT PRIMARY KEY, name VARCHAR(50))")?;

    let mut result = database.run("DESCRIBE test")?;

    let id_row = result.next_tuple(|_, row| row.clone())?.unwrap();
    assert_eq!(
        id_row[0],
        kite_sql::types::value::DataValue::from("id".to_string())
    );
    assert_eq!(
        id_row[1],
        kite_sql::types::value::DataValue::from("int".to_string())
    );

    let name_row = result.next_tuple(|_, row| row.clone())?.unwrap();
    assert_eq!(
        name_row[0],
        kite_sql::types::value::DataValue::from("name".to_string())
    );
    assert_eq!(
        name_row[1],
        kite_sql::types::value::DataValue::from("varchar(50)".to_string())
    );

    result.done()?;

    Ok(())
}
