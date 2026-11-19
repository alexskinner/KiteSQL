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

        // ROUND: single registration handles both 1 and 2 args via accepts_any_args
        registry.register_scalar(Round::new_variadic())?;

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
        if args.len() != 1 {
            return Err(DatabaseError::InvalidValue(format!(
                "LENGTH expects 1 argument, got {}",
                args.len()
            )));
        }
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

    fn accepts_any_args(&self) -> bool {
        true
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

        // Empty substring always matches at the starting position
        if sub.is_empty() {
            return Ok(DataValue::Int32(start_char_pos as i32));
        }

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

#[derive(Debug)]
pub struct Left {
    summary: FunctionSummary,
}

impl Left {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "left".into(),
                arg_types: vec![TEXT, LogicalType::Integer],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Left {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let len = arena.expression(args[1]).eval(arena, tuple)?;

        if str_val.is_none() || len.is_null() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let n = match len.as_ref() {
            DataValue::Int32(l) => *l.max(&0) as usize,
            _ => return Err(DatabaseError::InvalidValue("length must be integer".into())),
        };

        let chars: Vec<char> = s.chars().collect();
        let result: String = chars.iter().take(n).collect();
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

/// `RIGHT(str, len)`: returns rightmost len characters.
#[derive(Debug)]
pub struct Right {
    summary: FunctionSummary,
}

impl Right {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "right".into(),
                arg_types: vec![TEXT, LogicalType::Integer],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Right {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let len = arena.expression(args[1]).eval(arena, tuple)?;

        if str_val.is_none() || len.is_null() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let n = match len.as_ref() {
            DataValue::Int32(l) => *l.max(&0) as usize,
            _ => return Err(DatabaseError::InvalidValue("length must be integer".into())),
        };

        let chars: Vec<char> = s.chars().collect();
        let start = if chars.len() > n { chars.len() - n } else { 0 };
        let result: String = chars[start..].iter().collect();
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

/// `LPAD(str, len, padstr)`: left-pads str to len characters with padstr.
#[derive(Debug)]
pub struct Lpad {
    summary: FunctionSummary,
}

impl Lpad {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "lpad".into(),
                arg_types: vec![TEXT, LogicalType::Integer, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Lpad {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let len = arena.expression(args[1]).eval(arena, tuple)?;
        let pad = eval_text(args[2], arena, tuple)?;

        if str_val.is_none() || len.is_null() || pad.is_none() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let target_len = match len.as_ref() {
            DataValue::Int32(l) if *l >= 0 => *l as usize,
            _ => return Ok(DataValue::Null),
        };
        let padstr = pad.unwrap();

        if padstr.is_empty() {
            return Ok(DataValue::Null);
        }

        let chars: Vec<char> = s.chars().collect();
        if chars.len() >= target_len {
            let result: String = chars.iter().take(target_len).collect();
            return Ok(DataValue::from(result));
        }

        let pad_chars: Vec<char> = padstr.chars().collect();
        let pad_needed = target_len - chars.len();
        let mut result = String::new();
        for i in 0..pad_needed {
            result.push(pad_chars[i % pad_chars.len()]);
        }
        result.push_str(&s);
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

/// `RPAD(str, len, padstr)`: right-pads str to len characters with padstr.
#[derive(Debug)]
pub struct Rpad {
    summary: FunctionSummary,
}

impl Rpad {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "rpad".into(),
                arg_types: vec![TEXT, LogicalType::Integer, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Rpad {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let len = arena.expression(args[1]).eval(arena, tuple)?;
        let pad = eval_text(args[2], arena, tuple)?;

        if str_val.is_none() || len.is_null() || pad.is_none() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let target_len = match len.as_ref() {
            DataValue::Int32(l) if *l >= 0 => *l as usize,
            _ => return Ok(DataValue::Null),
        };
        let padstr = pad.unwrap();

        if padstr.is_empty() {
            return Ok(DataValue::Null);
        }

        let chars: Vec<char> = s.chars().collect();
        if chars.len() >= target_len {
            let result: String = chars.iter().take(target_len).collect();
            return Ok(DataValue::from(result));
        }

        let pad_chars: Vec<char> = padstr.chars().collect();
        let pad_needed = target_len - chars.len();
        let mut result = s.clone();
        for i in 0..pad_needed {
            result.push(pad_chars[i % pad_chars.len()]);
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

/// `REPEAT(str, count)`: repeats str count times.
#[derive(Debug)]
pub struct Repeat {
    summary: FunctionSummary,
}

impl Repeat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "repeat".into(),
                arg_types: vec![TEXT, LogicalType::Integer],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Repeat {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let str_val = eval_text(args[0], arena, tuple)?;
        let count = arena.expression(args[1]).eval(arena, tuple)?;

        if str_val.is_none() || count.is_null() {
            return Ok(DataValue::Null);
        }

        let s = str_val.unwrap();
        let n = match count.as_ref() {
            DataValue::Int32(c) if *c >= 0 => *c as usize,
            _ => 0,
        };

        Ok(DataValue::from(s.repeat(n)))
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

/// `REVERSE(str)`: reverses the characters in str.
#[derive(Debug)]
pub struct Reverse {
    summary: FunctionSummary,
}

impl Reverse {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "reverse".into(),
                arg_types: vec![TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Reverse {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        match eval_text(args[0], arena, tuple)? {
            Some(s) => Ok(DataValue::from(s.chars().rev().collect::<String>())),
            None => Ok(DataValue::Null),
        }
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

/// `LTRIM(str)`: removes leading whitespace from str.
#[derive(Debug)]
pub struct Ltrim {
    summary: FunctionSummary,
}

impl Ltrim {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "ltrim".into(),
                arg_types: vec![TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Ltrim {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        match eval_text(args[0], arena, tuple)? {
            Some(s) => Ok(DataValue::from(s.trim_start().to_string())),
            None => Ok(DataValue::Null),
        }
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

/// `RTRIM(str)`: removes trailing whitespace from str.
#[derive(Debug)]
pub struct Rtrim {
    summary: FunctionSummary,
}

impl Rtrim {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "rtrim".into(),
                arg_types: vec![TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Rtrim {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        match eval_text(args[0], arena, tuple)? {
            Some(s) => Ok(DataValue::from(s.trim_end().to_string())),
            None => Ok(DataValue::Null),
        }
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

/// `HEX(str_or_num)`: returns the hexadecimal representation.
/// For strings, converts bytes to hex. For integers, returns the hex value (negatives in two's complement).
#[derive(Debug)]
pub struct Hex {
    summary: FunctionSummary,
}

impl Hex {
    pub fn new(arg_type: LogicalType) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "hex".into(),
                arg_types: vec![arg_type],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Hex {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.len() != 1 {
            return Err(DatabaseError::InvalidValue(format!(
                "HEX expects 1 argument, got {}",
                args.len()
            )));
        }
        let value = arena.expression(args[0]).eval(arena, tuple)?;
        if value.is_null() {
            return Ok(DataValue::Null);
        }

        let hex = match value.as_ref() {
            DataValue::Boolean(b) => format!("{:X}", if *b { 1 } else { 0 }),
            DataValue::Int8(i) => format!("{:X}", *i as i64 as u64),
            DataValue::Int16(i) => format!("{:X}", *i as i64 as u64),
            DataValue::Int32(i) => format!("{:X}", *i as i64 as u64),
            DataValue::Int64(i) => format!("{:X}", *i as u64),
            DataValue::UInt8(i) => format!("{:X}", *i as u64),
            DataValue::UInt16(i) => format!("{:X}", *i as u64),
            DataValue::UInt32(i) => format!("{:X}", *i as u64),
            DataValue::UInt64(i) => format!("{:X}", *i),
            DataValue::Float32(f) => format!("{:X}", f.0.round() as i64 as u64),
            DataValue::Float64(f) => format!("{:X}", f.0.round() as i64 as u64),
            _ => match eval_value_as_text(args[0], arena, tuple)? {
                Some(s) => s.bytes().map(|b| format!("{:02X}", b)).collect(),
                None => return Ok(DataValue::Null),
            },
        };
        Ok(DataValue::from(hex))
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

    fn accepts_any_args(&self) -> bool {
        true
    }
}

/// `UUID()`: generates a version 4 UUID.
#[derive(Debug)]
pub struct Uuid {
    summary: FunctionSummary,
}

impl Uuid {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "uuid".into(),
                arg_types: vec![],
                persistence: PersistencePolicy::Transient,
            },
        })
    }
}

impl ScalarFunctionImpl for Uuid {
    fn eval(
        &self,
        _args: &[ExprRef],
        _arena: &(dyn MetaArena + '_),
        _tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        let uuid = uuid::Uuid::new_v4();
        Ok(DataValue::from(uuid.to_string()))
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

/// `GREATEST(val1, val2, ...)`: returns the largest value. NULL if any argument is NULL.
/// Works with numeric and text types.
#[derive(Debug)]
pub struct Greatest {
    summary: FunctionSummary,
}

impl Greatest {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "greatest".into(),
                arg_types: vec![TEXT, TEXT], // Placeholder, actual count checked at runtime
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Greatest {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.len() < 2 {
            return Err(DatabaseError::InvalidValue(
                "GREATEST requires at least 2 arguments".into(),
            ));
        }

        // Evaluate all arguments
        let mut values = Vec::new();
        for arg in args {
            let value = arena.expression(*arg).eval(arena, tuple)?;
            if value.is_null() {
                return Ok(DataValue::Null);
            }
            values.push(value.into_owned());
        }

        // Check if all arguments are numeric
        let all_numeric = values.iter().all(|v| {
            matches!(
                v,
                DataValue::Int8(_)
                    | DataValue::Int16(_)
                    | DataValue::Int32(_)
                    | DataValue::Int64(_)
                    | DataValue::UInt8(_)
                    | DataValue::UInt16(_)
                    | DataValue::UInt32(_)
                    | DataValue::UInt64(_)
                    | DataValue::Float32(_)
                    | DataValue::Float64(_)
            )
        });

        if all_numeric {
            // Numeric comparison: convert to f64
            let mut max_numeric = f64::NEG_INFINITY;
            for v in &values {
                let num = match v {
                    DataValue::Int8(i) => *i as f64,
                    DataValue::Int16(i) => *i as f64,
                    DataValue::Int32(i) => *i as f64,
                    DataValue::Int64(i) => *i as f64,
                    DataValue::UInt8(i) => *i as f64,
                    DataValue::UInt16(i) => *i as f64,
                    DataValue::UInt32(i) => *i as f64,
                    DataValue::UInt64(i) => *i as f64,
                    DataValue::Float32(f) => f.0 as f64,
                    DataValue::Float64(f) => f.0,
                    _ => unreachable!(),
                };
                if num > max_numeric {
                    max_numeric = num;
                }
            }
            use ordered_float::OrderedFloat;
            Ok(DataValue::Float64(OrderedFloat(max_numeric)))
        } else {
            // String comparison
            let mut max_value = values.remove(0);
            for value in values {
                if value > max_value {
                    max_value = value;
                }
            }
            Ok(max_value)
        }
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Double // Return Double for mixed numeric, varies otherwise
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }

    fn accepts_any_args(&self) -> bool {
        true // Accept any types, compare appropriately
    }
}

/// `LEAST(val1, val2, ...)`: returns the smallest value. NULL if any argument is NULL.
/// Works with numeric and text types.
#[derive(Debug)]
pub struct Least {
    summary: FunctionSummary,
}

impl Least {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "least".into(),
                arg_types: vec![TEXT, TEXT], // Placeholder, actual count checked at runtime
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Least {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.len() < 2 {
            return Err(DatabaseError::InvalidValue(
                "LEAST requires at least 2 arguments".into(),
            ));
        }

        // Evaluate all arguments
        let mut values = Vec::new();
        for arg in args {
            let value = arena.expression(*arg).eval(arena, tuple)?;
            if value.is_null() {
                return Ok(DataValue::Null);
            }
            values.push(value.into_owned());
        }

        // Check if all arguments are numeric
        let all_numeric = values.iter().all(|v| {
            matches!(
                v,
                DataValue::Int8(_)
                    | DataValue::Int16(_)
                    | DataValue::Int32(_)
                    | DataValue::Int64(_)
                    | DataValue::UInt8(_)
                    | DataValue::UInt16(_)
                    | DataValue::UInt32(_)
                    | DataValue::UInt64(_)
                    | DataValue::Float32(_)
                    | DataValue::Float64(_)
            )
        });

        if all_numeric {
            // Numeric comparison: convert to f64
            let mut min_numeric = f64::INFINITY;
            for v in &values {
                let num = match v {
                    DataValue::Int8(i) => *i as f64,
                    DataValue::Int16(i) => *i as f64,
                    DataValue::Int32(i) => *i as f64,
                    DataValue::Int64(i) => *i as f64,
                    DataValue::UInt8(i) => *i as f64,
                    DataValue::UInt16(i) => *i as f64,
                    DataValue::UInt32(i) => *i as f64,
                    DataValue::UInt64(i) => *i as f64,
                    DataValue::Float32(f) => f.0 as f64,
                    DataValue::Float64(f) => f.0,
                    _ => unreachable!(),
                };
                if num < min_numeric {
                    min_numeric = num;
                }
            }
            use ordered_float::OrderedFloat;
            Ok(DataValue::Float64(OrderedFloat(min_numeric)))
        } else {
            // String comparison
            let mut min_value = values.remove(0);
            for value in values {
                if value < min_value {
                    min_value = value;
                }
            }
            Ok(min_value)
        }
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Double // Return Double for mixed numeric, varies otherwise
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }

    fn accepts_any_args(&self) -> bool {
        true // Accept any types, compare appropriately
    }
}

/// `IFNULL(expr1, expr2)`: returns expr1 if not NULL, otherwise expr2.
#[derive(Debug)]
pub struct Ifnull {
    summary: FunctionSummary,
}

impl Ifnull {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "ifnull".into(),
                arg_types: vec![TEXT, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Ifnull {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        match eval_text(args[0], arena, tuple)? {
            Some(v) => Ok(DataValue::from(v)),
            None => match eval_text(args[1], arena, tuple)? {
                Some(v) => Ok(DataValue::from(v)),
                None => Ok(DataValue::Null),
            },
        }
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

/// `ROUND(x[, d])`: rounds x to d decimal places (default 0). Uses MySQL half-away-from-zero rounding.
/// Supports Float, Decimal, and Integer types.
#[derive(Debug)]
pub struct Round {
    summary: FunctionSummary,
}

impl Round {
    pub fn new(arg_type: LogicalType, with_decimals: bool) -> Arc<Self> {
        let arg_types = if with_decimals {
            vec![arg_type, LogicalType::Integer]
        } else {
            vec![arg_type]
        };
        Arc::new(Self {
            summary: FunctionSummary {
                name: "round".into(),
                arg_types,
                persistence: PersistencePolicy::Durable,
            },
        })
    }

    pub fn new_variadic() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "round".into(),
                arg_types: vec![LogicalType::Double], // Placeholder, accepts 1 or 2 args
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Round {
    fn eval(
        &self,
        args: &[ExprRef],
        arena: &(dyn MetaArena + '_),
        tuple: Option<&dyn Input>,
    ) -> Result<DataValue, DatabaseError> {
        if args.is_empty() || args.len() > 2 {
            return Err(DatabaseError::InvalidValue(format!(
                "ROUND expects 1 or 2 arguments, got {}",
                args.len()
            )));
        }

        let x_val = arena.expression(args[0]).eval(arena, tuple)?;
        if x_val.is_null() {
            return Ok(DataValue::Null);
        }

        let d = if args.len() == 2 {
            let d_val = arena.expression(args[1]).eval(arena, tuple)?;
            if d_val.is_null() {
                return Ok(DataValue::Null);
            }
            match d_val.as_ref() {
                DataValue::Int32(i) => *i,
                DataValue::Int64(i) => *i as i32,
                _ => {
                    return Err(DatabaseError::InvalidValue(
                        "decimals must be integer".into(),
                    ))
                }
            }
        } else {
            0
        };

        // Clamp d to prevent overflow and excessive computation
        let d = d.max(-30).min(30);

        match x_val.as_ref() {
            DataValue::Int32(i) => {
                if d >= 0 {
                    Ok(DataValue::Int32(*i))
                } else {
                    // Widen to i128 to handle overflow
                    let val = *i as i128;
                    let abs_d = (-d) as u32;
                    let divisor = 10_i128.pow(abs_d);
                    let rounded = ((val as f64 / divisor as f64).round() as i128) * divisor;
                    // Clamp to i32 range
                    if rounded > i32::MAX as i128 || rounded < i32::MIN as i128 {
                        Ok(DataValue::Int32(0))
                    } else {
                        Ok(DataValue::Int32(rounded as i32))
                    }
                }
            }
            DataValue::Int64(i) => {
                if d >= 0 {
                    Ok(DataValue::Int64(*i))
                } else {
                    // Widen to i128 to handle overflow
                    let val = *i as i128;
                    let abs_d = (-d) as u32;
                    let divisor = 10_i128.pow(abs_d);
                    let rounded = ((val as f64 / divisor as f64).round() as i128) * divisor;
                    // Clamp to i64 range
                    if rounded > i64::MAX as i128 || rounded < i64::MIN as i128 {
                        Ok(DataValue::Int64(0))
                    } else {
                        Ok(DataValue::Int64(rounded as i64))
                    }
                }
            }
            DataValue::Decimal(dec) => {
                use rust_decimal::prelude::*;
                if d >= 0 {
                    let rounded = dec
                        .round_dp_with_strategy(d as u32, RoundingStrategy::MidpointAwayFromZero);
                    Ok(DataValue::Decimal(rounded))
                } else {
                    // For negative d, scale by 10^-d using Decimal
                    let abs_d = (-d) as u32;
                    let scale_factor = Decimal::from_i128_with_scale(10_i128.pow(abs_d), 0);
                    let scaled = *dec / scale_factor;
                    let rounded =
                        scaled.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
                    Ok(DataValue::Decimal(rounded * scale_factor))
                }
            }
            DataValue::Float32(f) => {
                let val = f.0 as f64;
                // Check for special values
                if !val.is_finite() {
                    return Ok(DataValue::Float32(*f));
                }
                // Clamp d to prevent NaN
                let multiplier = 10_f64.powi(d);
                if !multiplier.is_finite() {
                    return Ok(DataValue::Float32(*f));
                }
                let result = (val * multiplier).round() / multiplier;
                use ordered_float::OrderedFloat;
                if result.is_finite() {
                    Ok(DataValue::Float32(OrderedFloat(result as f32)))
                } else {
                    Ok(DataValue::Float32(*f))
                }
            }
            DataValue::Float64(f) => {
                // Check for special values
                if !f.0.is_finite() {
                    return Ok(DataValue::Float64(*f));
                }
                // Clamp d to prevent NaN
                let multiplier = 10_f64.powi(d);
                if !multiplier.is_finite() {
                    return Ok(DataValue::Float64(*f));
                }
                let result = (f.0 * multiplier).round() / multiplier;
                use ordered_float::OrderedFloat;
                if result.is_finite() {
                    Ok(DataValue::Float64(OrderedFloat(result)))
                } else {
                    Ok(DataValue::Float64(*f))
                }
            }
            _ => Err(DatabaseError::InvalidValue(
                "ROUND expects numeric argument".into(),
            )),
        }
    }

    fn return_type(&self) -> &LogicalType {
        &self.summary.arg_types[0]
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }

    fn accepts_any_args(&self) -> bool {
        true
    }
}
