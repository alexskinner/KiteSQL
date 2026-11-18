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
use kite_sql::dialect::SqlDialect;
use kite_sql::errors::DatabaseError;
use kite_sql::types::{CharLengthUnits, LogicalType};
use sqlparser::ast::DataType;
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect};
use std::sync::Arc;

/// A toy custom dialect for testing that maps "CUSTOMTEXT" to Varchar.
#[derive(Debug)]
struct ToyDialect {
    parser: GenericDialect,
}

impl ToyDialect {
    fn new() -> Self {
        Self {
            parser: GenericDialect {},
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
