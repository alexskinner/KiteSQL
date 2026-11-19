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

//! MySQL-specific scalar functions, installed by [`MySqlFunctions`].

use kite_sql::errors::DatabaseError;
use kite_sql::expression::function::scala::ScalarFunctionImpl;
use kite_sql::expression::function::{FunctionSummary, PersistencePolicy};
use kite_sql::planner::{ExprRef, MetaArena};
use kite_sql::types::tuple::Input;
use kite_sql::types::value::DataValue;
use kite_sql::types::{CharLengthUnits, LogicalType};
use kite_sql::{FunctionPack, FunctionRegistry};
use std::sync::Arc;

const TEXT: LogicalType = LogicalType::Varchar(None, CharLengthUnits::Characters);

/// The MySQL function pack. `with_mysql()` installs it.
#[derive(Debug, Default)]
pub struct MySqlFunctions;

impl FunctionPack for MySqlFunctions {
    fn name(&self) -> &str {
        "mysql"
    }

    fn install(&self, registry: &mut FunctionRegistry) -> Result<(), DatabaseError> {
        registry.register_scalar(Replace::new())?;

        // LENGTH: accepts any type (TEXT signature, converts numerics to string)
        registry.register_scalar(Length::new(TEXT))?;

        // HEX: accepts any type (TEXT signature, handles numerics specially)
        registry.register_scalar(Hex::new(TEXT))?;

        // CONCAT: truly variadic with runtime arity check (minimum 1 arg)
        registry.register_scalar(Concat::new())?;

        // CONCAT_WS: truly variadic with runtime arity check (minimum 2 args)
        registry.register_scalar(ConcatWs::new())?;

        registry.register_scalar(SubstringIndex::new())?;
        registry.register_scalar(Locate::new(2))?;
        registry.register_scalar(Locate::new(3))?;
        registry.register_scalar(Instr::new())?;
        // POSITION is handled by core evaluator (PR #38), don't register here
        registry.register_scalar(Left::new())?;
        registry.register_scalar(Right::new())?;
        registry.register_scalar(Lpad::new())?;
        registry.register_scalar(Rpad::new())?;
        registry.register_scalar(Repeat::new())?;
        registry.register_scalar(Reverse::new())?;
        registry.register_scalar(Ltrim::new())?;
        registry.register_scalar(Rtrim::new())?;
        registry.register_scalar(Uuid::new())?;

        // GREATEST/LEAST: truly variadic with runtime arity check (minimum 2 args)
        registry.register_scalar(Greatest::new())?;
        registry.register_scalar(Least::new())?;

        // ROUND: accepts any numeric type via accepts_any_args
        registry.register_scalar(Round::new(LogicalType::Double, false))?;
        registry.register_scalar(Round::new(LogicalType::Double, true))?;

        Ok(())
    }
}

fn eval_text(
    arg: ExprRef,
    arena: &(dyn MetaArena + '_),
    tuple: Option<&dyn Input>,
) -> Result<Option<String>, DatabaseError> {
    let value = arena.expression(arg).eval(arena, tuple)?;
    if value.is_null() {
        return Ok(None);
    }
    let value = value.into_owned().cast(&TEXT)?;
    Ok(value.utf8().map(str::to_string))
}

/// Evaluate an argument as text, converting numbers to their string representation (MySQL formatting).
/// Booleans convert to "1" or "0".
fn eval_value_as_text(
    arg: ExprRef,
    arena: &(dyn MetaArena + '_),
    tuple: Option<&dyn Input>,
) -> Result<Option<String>, DatabaseError> {
    let value = arena.expression(arg).eval(arena, tuple)?;
    if value.is_null() {
        return Ok(None);
    }
    match value.as_ref() {
        DataValue::Boolean(b) => Ok(Some(if *b { "1" } else { "0" }.to_string())),
        DataValue::Int8(i) => Ok(Some(i.to_string())),
        DataValue::Int16(i) => Ok(Some(i.to_string())),
        DataValue::Int32(i) => Ok(Some(i.to_string())),
        DataValue::Int64(i) => Ok(Some(i.to_string())),
        DataValue::UInt8(i) => Ok(Some(i.to_string())),
        DataValue::UInt16(i) => Ok(Some(i.to_string())),
        DataValue::UInt32(i) => Ok(Some(i.to_string())),
        DataValue::UInt64(i) => Ok(Some(i.to_string())),
        DataValue::Float32(f) => Ok(Some(f.0.to_string())),
        DataValue::Float64(f) => Ok(Some(f.0.to_string())),
        #[cfg(feature = "decimal")]
        DataValue::Decimal(d) => Ok(Some(d.to_string())),
        _ => {
            let value = value.into_owned().cast(&TEXT)?;
            Ok(value.utf8().map(str::to_string))
        }
    }
}

/// `REPLACE(str, from_str, to_str)`: every occurrence of `from_str` in `str`, matched
/// case-sensitively, is replaced with `to_str`. Returns NULL if any argument is NULL, and `str`
/// unchanged when `from_str` is empty.
#[derive(Debug)]
pub struct Replace {
    summary: FunctionSummary,
}

impl Replace {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "replace".into(),
                arg_types: vec![TEXT, TEXT, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Replace {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let mut texts = Vec::with_capacity(3);
        for arg in args {
            match eval_text(*arg, arena, tuple)? {
                Some(text) => texts.push(text),
                None => return Ok(DataValue::Null),
            }
        }
        let [input, from, to] = <[String; 3]>::try_from(texts)
            .map_err(|_| DatabaseError::MisMatch("number of replace() parameters", "3"))?;
        if from.is_empty() {
            return Ok(DataValue::from(input));
        }
        Ok(DataValue::from(input.replace(&from, &to)))
    }

    fn return_type(&self) -> &LogicalType {
        &TEXT
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `LENGTH(str)`: returns the byte length of `str` in UTF-8 encoding. NULL if argument is NULL.
/// Accepts numeric arguments (converted to string first).
#[derive(Debug)]
pub struct Length {
    summary: FunctionSummary,
}

impl Length {
    pub fn new(arg_type: LogicalType) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "length".into(),
                arg_types: vec![arg_type],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Length {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        match eval_value_as_text(args[0], arena, tuple)? {
            Some(s) => Ok(DataValue::Int32(s.len() as i32)),
            None => Ok(DataValue::Null),
        }
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Integer
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `CONCAT(str1, str2, ...)`: concatenates all arguments. Returns NULL if any argument is NULL.
#[derive(Debug)]
pub struct Concat {
    summary: FunctionSummary,
}

impl Concat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "concat".into(),
                arg_types: vec![TEXT], // Placeholder, actual count checked at runtime
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Concat {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.is_empty() {
            return Err(DatabaseError::InvalidValue(
                "CONCAT requires at least 1 argument".into(),
            ));
        }
        let mut result = String::new();
        for arg in args {
            match eval_value_as_text(*arg, arena, tuple)? {
                Some(s) => result.push_str(&s),
                None => return Ok(DataValue::Null),
            }
        }
        Ok(DataValue::from(result))
    }

    fn return_type(&self) -> &LogicalType {
        &TEXT
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `CONCAT_WS(separator, str1, str2, ...)`: concatenates strings with separator. Skips NULL values.
/// Returns NULL if separator is NULL.
#[derive(Debug)]
pub struct ConcatWs {
    summary: FunctionSummary,
}

impl ConcatWs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "concat_ws".into(),
                arg_types: vec![TEXT, TEXT], // Placeholder, actual count checked at runtime
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for ConcatWs {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.len() < 2 {
            return Err(DatabaseError::InvalidValue(
                "CONCAT_WS requires at least 2 arguments".into(),
            ));
        }
        let separator = eval_text(args[0], arena, tuple)?;
        if separator.is_none() {
            return Ok(DataValue::Null);
        }
        let sep = separator.unwrap();

        let mut parts = Vec::new();
        for arg in &args[1..] {
            if let Some(s) = eval_value_as_text(*arg, arena, tuple)? {
                parts.push(s);
            }
        }
        Ok(DataValue::from(parts.join(&sep)))
    }

    fn return_type(&self) -> &LogicalType {
        &TEXT
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `SUBSTRING_INDEX(str, delim, count)`: returns substring before count occurrences of delim.
#[derive(Debug)]
pub struct SubstringIndex {
    summary: FunctionSummary,
}

impl SubstringIndex {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "substring_index".into(),
                arg_types: vec![TEXT, TEXT, LogicalType::Integer],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for SubstringIndex {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let delim = eval_text(args[1], arena, tuple)?;
        let count = arena.expression(args[2]).eval(arena, tuple)?;

        if str_val.is_none() || delim.is_none() || count.is_null() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let d = delim.unwrap();
        let cnt = match count.as_ref() {
            DataValue::Int32(c) => *c,
            _ => return Err(DatabaseError::InvalidValue("count must be integer".into())),
        };

        if cnt == 0 || d.is_empty() {
            return Ok(DataValue::from(String::new()));
        }

        let result = if cnt > 0 {
            let mut positions = Vec::new();
            let mut start = 0;
            while let Some(pos) = s[start..].find(&d) {
                positions.push(start + pos);
                if positions.len() >= cnt as usize {
                    break;
                }
                start += pos + d.len();
            }
            if positions.len() < cnt as usize {
                s.clone()
            } else {
                s[..positions[cnt as usize - 1]].to_string()
            }
        } else {
            let mut positions = Vec::new();
            let mut start = 0;
            while let Some(pos) = s[start..].find(&d) {
                positions.push(start + pos);
                start += pos + d.len();
            }
            let abs_cnt = (-cnt) as usize;
            if positions.len() < abs_cnt {
                s.clone()
            } else {
                let idx = positions.len() - abs_cnt;
                s[(positions[idx] + d.len())..].to_string()
            }
        };

        Ok(DataValue::from(result))
    }

    fn return_type(&self) -> &LogicalType {
        &TEXT
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `LOCATE(substr, str[, pos])`: returns 1-based character position of substr in str, or 0 if not found.
/// Case-insensitive comparison following MySQL's default collation.
#[derive(Debug)]
pub struct Locate {
    summary: FunctionSummary,
}

impl Locate {
    pub fn new(arg_count: usize) -> Arc<Self> {
        let arg_types = if arg_count == 2 {
            vec![TEXT, TEXT]
        } else {
            vec![TEXT, TEXT, LogicalType::Integer]
        };
        Arc::new(Self {
            summary: FunctionSummary {
                name: "locate".into(),
                arg_types,
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Locate {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let substr = eval_text(args[0], arena, tuple)?;
        let str_val = eval_text(args[1], arena, tuple)?;

        if substr.is_none() || str_val.is_none() {
            return Ok(DataValue::Null);
        }

        let sub = substr.unwrap().to_lowercase();
        let s = str_val.unwrap();
        let s_lower = s.to_lowercase();

        let start_char_pos = if args.len() == 3 {
            let pos = arena.expression(args[2]).eval(arena, tuple)?;
            if pos.is_null() {
                return Ok(DataValue::Null);
            }
            match pos.as_ref() {
                DataValue::Int32(p) => {
                    if *p < 1 {
                        return Ok(DataValue::Int32(0));
                    }
                    *p as usize
                }
                _ => {
                    return Err(DatabaseError::InvalidValue(
                        "position must be integer".into(),
                    ))
                }
            }
        } else {
            1
        };

        let char_count = s.chars().count();
        if start_char_pos == 0 || start_char_pos > char_count {
            return Ok(DataValue::Int32(0));
        }

        let start_byte_offset = s
            .char_indices()
            .nth(start_char_pos - 1)
            .map(|(idx, _)| idx)
            .unwrap_or(s.len());

        let search = &s_lower[start_byte_offset..];
        if let Some(byte_pos) = search.find(&sub) {
            let found_byte_offset = start_byte_offset + byte_pos;
            let char_pos = s[..found_byte_offset].chars().count() + 1;
            Ok(DataValue::Int32(char_pos as i32))
        } else {
            Ok(DataValue::Int32(0))
        }
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Integer
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}

/// `INSTR(str, substr)`: returns 1-based character position of substr in str, or 0 if not found.
/// Case-insensitive comparison following MySQL's default collation.
#[derive(Debug)]
pub struct Instr {
    summary: FunctionSummary,
}

impl Instr {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "instr".into(),
                arg_types: vec![TEXT, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Instr {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let substr = eval_text(args[1], arena, tuple)?;

        if str_val.is_none() || substr.is_none() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let s_lower = s.to_lowercase();
        let sub = substr.unwrap().to_lowercase();

        if let Some(byte_pos) = s_lower.find(&sub) {
            let char_pos = s[..byte_pos].chars().count() + 1;
            Ok(DataValue::Int32(char_pos as i32))
        } else {
            Ok(DataValue::Int32(0))
        }
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Integer
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}
