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
        registry.register_scalar(Length::new())?;
        registry.register_scalar(Concat::new(2))?;
        registry.register_scalar(Concat::new(3))?;
        registry.register_scalar(Concat::new(4))?;
        registry.register_scalar(Concat::new(5))?;
        registry.register_scalar(ConcatWs::new(2))?;
        registry.register_scalar(ConcatWs::new(3))?;
        registry.register_scalar(ConcatWs::new(4))?;
        registry.register_scalar(ConcatWs::new(5))?;
        registry.register_scalar(SubstringIndex::new())?;
        registry.register_scalar(Locate::new(2))?;
        registry.register_scalar(Locate::new(3))?;
        registry.register_scalar(Instr::new())?;
        registry.register_scalar(Position::new())?;
        registry.register_scalar(Left::new())?;
        registry.register_scalar(Right::new())?;
        registry.register_scalar(Lpad::new())?;
        registry.register_scalar(Rpad::new())?;
        registry.register_scalar(Repeat::new())?;
        registry.register_scalar(Reverse::new())?;
        registry.register_scalar(Ltrim::new())?;
        registry.register_scalar(Rtrim::new())?;
        registry.register_scalar(Hex::new())?;
        registry.register_scalar(Uuid::new())?;
        registry.register_scalar(Greatest::new(2))?;
        registry.register_scalar(Greatest::new(3))?;
        registry.register_scalar(Greatest::new(4))?;
        registry.register_scalar(Least::new(2))?;
        registry.register_scalar(Least::new(3))?;
        registry.register_scalar(Least::new(4))?;
        registry.register_scalar(Ifnull::new())?;
        registry.register_scalar(Round::new(1))?;
        registry.register_scalar(Round::new(2))?;
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
#[derive(Debug)]
pub struct Length {
    summary: FunctionSummary,
}

impl Length {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "length".into(),
                arg_types: vec![TEXT],
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
        match eval_text(args[0], arena, tuple)? {
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
    pub fn new(arg_count: usize) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "concat".into(),
                arg_types: vec![TEXT; arg_count],
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
        let mut result = String::new();
        for arg in args {
            match eval_text(*arg, arena, tuple)? {
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
    pub fn new(arg_count: usize) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "concat_ws".into(),
                arg_types: vec![TEXT; arg_count],
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
        let sep = match eval_text(args[0], arena, tuple)? {
            Some(s) => s,
            None => return Ok(DataValue::Null),
        };
        let mut parts = Vec::new();
        for arg in &args[1..] {
            if let Some(s) = eval_text(*arg, arena, tuple)? {
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

/// `LOCATE(substr, str[, pos])`: returns 1-based position of substr in str, or 0 if not found.
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

        let sub = substr.unwrap();
        let s = str_val.unwrap();

        let start_pos = if args.len() == 3 {
            let pos = arena.expression(args[2]).eval(arena, tuple)?;
            if pos.is_null() {
                return Ok(DataValue::Null);
            }
            match pos.as_ref() {
                DataValue::Int32(p) => *p.max(&1) as usize,
                _ => {
                    return Err(DatabaseError::InvalidValue(
                        "position must be integer".into(),
                    ))
                }
            }
        } else {
            1
        };

        if start_pos == 0 || start_pos > s.len() {
            return Ok(DataValue::Int32(0));
        }

        let search = &s[(start_pos - 1)..];
        if let Some(pos) = search.find(&sub) {
            Ok(DataValue::Int32((start_pos + pos) as i32))
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

/// `INSTR(str, substr)`: returns 1-based position of substr in str, or 0 if not found.
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
        let sub = substr.unwrap();

        if let Some(pos) = s.find(&sub) {
            Ok(DataValue::Int32((pos + 1) as i32))
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

/// `POSITION(substr IN str)`: returns 1-based position of substr in str, or 0 if not found.
#[derive(Debug)]
pub struct Position {
    summary: FunctionSummary,
}

impl Position {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "position".into(),
                arg_types: vec![TEXT, TEXT],
                persistence: PersistencePolicy::Durable,
            },
        })
    }
}

impl ScalarFunctionImpl for Position {
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

        let sub = substr.unwrap();
        let s = str_val.unwrap();

        if let Some(pos) = s.find(&sub) {
            Ok(DataValue::Int32((pos + 1) as i32))
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

/// `LEFT(str, len)`: returns leftmost len characters.
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

/// `HEX(str)`: returns hexadecimal representation of str.
#[derive(Debug)]
pub struct Hex {
    summary: FunctionSummary,
}

impl Hex {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "hex".into(),
                arg_types: vec![TEXT],
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
        match eval_text(args[0], arena, tuple)? {
            Some(s) => {
                let hex: String = s.bytes().map(|b| format!("{:02X}", b)).collect();
                Ok(DataValue::from(hex))
            }
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

/// `GREATEST(val1, val2, ...)`: returns the greatest value.
#[derive(Debug)]
pub struct Greatest {
    summary: FunctionSummary,
}

impl Greatest {
    pub fn new(arg_count: usize) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "greatest".into(),
                arg_types: vec![TEXT; arg_count],
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
        let mut values = Vec::new();
        for arg in args {
            match eval_text(*arg, arena, tuple)? {
                Some(v) => values.push(v),
                None => return Ok(DataValue::Null),
            }
        }
        let max = values.into_iter().max().unwrap();
        Ok(DataValue::from(max))
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

/// `LEAST(val1, val2, ...)`: returns the smallest value.
#[derive(Debug)]
pub struct Least {
    summary: FunctionSummary,
}

impl Least {
    pub fn new(arg_count: usize) -> Arc<Self> {
        Arc::new(Self {
            summary: FunctionSummary {
                name: "least".into(),
                arg_types: vec![TEXT; arg_count],
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
        let mut values = Vec::new();
        for arg in args {
            match eval_text(*arg, arena, tuple)? {
                Some(v) => values.push(v),
                None => return Ok(DataValue::Null),
            }
        }
        let min = values.into_iter().min().unwrap();
        Ok(DataValue::from(min))
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

/// `ROUND(x[, d])`: rounds x to d decimal places (default 0). Uses half-away-from-zero.
#[derive(Debug)]
pub struct Round {
    summary: FunctionSummary,
}

impl Round {
    pub fn new(arg_count: usize) -> Arc<Self> {
        let arg_types = if arg_count == 1 {
            vec![LogicalType::Double]
        } else {
            vec![LogicalType::Double, LogicalType::Integer]
        };
        Arc::new(Self {
            summary: FunctionSummary {
                name: "round".into(),
                arg_types,
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
        let x_val = arena.expression(args[0]).eval(arena, tuple)?;
        if x_val.is_null() {
            return Ok(DataValue::Null);
        }

        let x = match x_val.as_ref() {
            DataValue::Float64(f) => f.0,
            DataValue::Float32(f) => f.0 as f64,
            DataValue::Int32(i) => *i as f64,
            DataValue::Int64(i) => *i as f64,
            _ => {
                return Err(DatabaseError::InvalidValue(
                    "ROUND expects numeric argument".into(),
                ))
            }
        };

        let d = if args.len() == 2 {
            let d_val = arena.expression(args[1]).eval(arena, tuple)?;
            if d_val.is_null() {
                return Ok(DataValue::Null);
            }
            match d_val.as_ref() {
                DataValue::Int32(i) => *i,
                _ => {
                    return Err(DatabaseError::InvalidValue(
                        "decimals must be integer".into(),
                    ))
                }
            }
        } else {
            0
        };

        let multiplier = 10_f64.powi(d);
        let result = (x * multiplier).round() / multiplier;

        use ordered_float::OrderedFloat;
        Ok(DataValue::Float64(OrderedFloat(result)))
    }

    fn return_type(&self) -> &LogicalType {
        &LogicalType::Double
    }

    fn summary(&self) -> &FunctionSummary {
        &self.summary
    }

    fn monotonicity(&self) -> Option<Vec<Option<bool>>> {
        None
    }
}
