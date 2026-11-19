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

use crate::errors::DatabaseError;
use crate::expression::function::table::{
    ArcTableFunctionImpl, TableFunctionCatalog, TableFunctionImpl,
};
use crate::function::abs::Abs;
use crate::function::char_length::CharLength;
#[cfg(feature = "time")]
use crate::function::current_date::CurrentDate;
#[cfg(feature = "time")]
use crate::function::current_timestamp::CurrentTimeStamp;
use crate::function::lower::Lower;
use crate::function::numbers::Numbers;
use crate::function::octet_length::OctetLength;
use crate::function::upper::Upper;
use crate::planner::TableArena;
use crate::types::tuple::Schema;
use std::sync::Arc;

pub fn install_core_functions(
    registry: &mut crate::function::registry::FunctionRegistry,
    table_arena: &mut TableArena,
) -> Result<(), DatabaseError> {
    for ty in Abs::types() {
        registry.load_scalar(Abs::new(ty));
    }
    #[cfg(feature = "decimal")]
    {
        registry.load_scalar(Abs::new(crate::types::LogicalType::Decimal(None, None)));
    }
    registry.load_scalar(CharLength::new("char_length".to_lowercase()));
    registry.load_scalar(CharLength::new("character_length".to_lowercase()));
    #[cfg(feature = "time")]
    registry.load_scalar(CurrentDate::new());
    #[cfg(feature = "time")]
    registry.load_scalar(CurrentTimeStamp::new());
    registry.load_scalar(Lower::new());
    registry.load_scalar(OctetLength::new());
    registry.load_scalar(Upper::new());

    let numbers = Numbers::new();
    let mut schema = Schema::new();
    numbers
        .as_ref()
        .output_schema_into(table_arena, &mut schema);
    let catalog = TableFunctionCatalog {
        inner: ArcTableFunctionImpl(numbers),
        schema,
    };
    registry.load_table(Arc::new(catalog));

    Ok(())
}
