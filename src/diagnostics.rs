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

//! Statement diagnostics: notes and warnings a statement raises while it still succeeds.
//!
//! Expression evaluators and executors have no session handle, so diagnostics go to a
//! per-thread, per-statement sink: parsing a statement ([`begin_statement`], called by
//! `prepare_all`) clears it and installs the dialect's [`StatementPolicy`]; anything the
//! statement raises while it is bound, planned and iterated is pushed here, and the caller
//! reads it back with [`take`] (or `Database::take_diagnostics`, which also maps each entry
//! through the dialect) once the statement's rows are drained. Statements run on the
//! thread that iterates them, so the sink belongs to that statement until the next one is
//! prepared on the same thread.
//!
//! Core raises dialect-neutral [`DiagnosticKind`]s with plain messages and no code; a
//! [`SqlDialect`](crate::dialect::SqlDialect) maps them to its own levels, codes and wording.
//! With the default [`StatementPolicy`] nothing that used to fail now succeeds: division by
//! zero and malformed numeric strings stay errors.

use crate::types::value::DataValue;
use std::cell::{Cell, RefCell};
use std::fmt;

/// Upper bound on diagnostics kept per statement (later ones are dropped).
pub const MAX_DIAGNOSTICS: usize = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticLevel {
    Note,
    Warning,
    Error,
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DiagnosticLevel::Note => "Note",
            DiagnosticLevel::Warning => "Warning",
            DiagnosticLevel::Error => "Error",
        })
    }
}

/// The numeric type a string was converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumericTarget {
    Integer,
    Double,
    Decimal,
}

impl fmt::Display for NumericTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NumericTarget::Integer => "INTEGER",
            NumericTarget::Double => "DOUBLE",
            NumericTarget::Decimal => "DECIMAL",
        })
    }
}

/// What happened, independent of any dialect's code or wording.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// Division or modulo by zero evaluated to NULL.
    DivisionByZero,
    /// A string converted to a number using only its leading numeric part (0 when it has none).
    TruncatedNumber {
        value: String,
        target: NumericTarget,
    },
    /// A value lost digits to fit the column it was stored in (e.g. DECIMAL scale).
    DataTruncated { column: String, row: usize },
    /// Raised by a dialect, function pack or embedding application with its own code.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    /// Dialect code; 0 until a dialect maps the diagnostic (core never assigns one).
    pub code: u32,
    pub message: String,
    pub kind: DiagnosticKind,
}

impl Diagnostic {
    pub fn new(level: DiagnosticLevel, kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Diagnostic {
            level,
            code: 0,
            message: message.into(),
            kind,
        }
    }

    pub fn division_by_zero() -> Self {
        Self::new(
            DiagnosticLevel::Warning,
            DiagnosticKind::DivisionByZero,
            "division by zero",
        )
    }

    pub fn truncated_number(value: &str, target: NumericTarget) -> Self {
        Self::new(
            DiagnosticLevel::Warning,
            DiagnosticKind::TruncatedNumber {
                value: value.to_string(),
                target,
            },
            format!("truncated incorrect {target} value: '{value}'"),
        )
    }

    pub fn data_truncated(column: &str, row: usize) -> Self {
        Self::new(
            DiagnosticLevel::Note,
            DiagnosticKind::DataTruncated {
                column: column.to_string(),
                row,
            },
            format!("data truncated for column '{column}' at row {row}"),
        )
    }
}

/// What division by zero evaluates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DivisionByZero {
    /// Fail the statement (integer and DECIMAL division; floating-point division keeps IEEE
    /// semantics).
    #[default]
    Error,
    /// NULL, with a [`DiagnosticKind::DivisionByZero`] warning, for every numeric type.
    NullWithWarning,
}

/// Per-statement evaluation choices a dialect makes (see `SqlDialect::statement_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatementPolicy {
    pub division_by_zero: DivisionByZero,
    /// Convert a malformed numeric string by its leading numeric part (leading whitespace
    /// skipped, 0 when there is none) with a [`DiagnosticKind::TruncatedNumber`] warning,
    /// instead of failing. Integer targets only take integer prefixes ("12abc"); a string
    /// with a fraction or exponent ("1.5x") still fails there. Never applied while a value
    /// is stored into a column (see [`strict_store`]).
    pub lenient_numeric_strings: bool,
}

thread_local! {
    static SINK: RefCell<Vec<Diagnostic>> = const { RefCell::new(Vec::new()) };
    static POLICY: Cell<StatementPolicy> = const {
        Cell::new(StatementPolicy {
            division_by_zero: DivisionByZero::Error,
            lenient_numeric_strings: false,
        })
    };
}

/// Starts a statement on this thread: clears its diagnostics and installs `policy`.
pub fn begin_statement(policy: StatementPolicy) {
    SINK.with(|s| s.borrow_mut().clear());
    POLICY.with(|p| p.set(policy));
}

/// The policy of the statement running on this thread.
pub fn policy() -> StatementPolicy {
    POLICY.with(|p| p.get())
}

/// Records a diagnostic for the statement running on this thread.
pub fn push(diagnostic: Diagnostic) {
    SINK.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() < MAX_DIAGNOSTICS {
            s.push(diagnostic);
        }
    });
}

/// Number of diagnostics the current statement has raised so far.
pub fn count() -> usize {
    SINK.with(|s| s.borrow().len())
}

/// Removes and returns the current statement's diagnostics (unmapped).
pub fn take() -> Vec<Diagnostic> {
    SINK.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Runs `f` with lenient numeric strings off: storing a value must not silently turn a
/// malformed string into a number.
pub fn strict_store<R>(f: impl FnOnce() -> R) -> R {
    let saved = policy();
    POLICY.with(|p| {
        p.set(StatementPolicy {
            lenient_numeric_strings: false,
            ..saved
        })
    });
    let r = f();
    POLICY.with(|p| p.set(saved));
    r
}

/// Division by zero under the current policy: `Ok(())` means "evaluate to NULL" (a warning
/// has been recorded), `Err` fails the statement.
pub fn division_by_zero() -> Result<(), crate::errors::DatabaseError> {
    match policy().division_by_zero {
        DivisionByZero::Error => Err(crate::errors::DatabaseError::InvalidValue(
            "division by zero".into(),
        )),
        DivisionByZero::NullWithWarning => {
            push(Diagnostic::division_by_zero());
            Ok(())
        }
    }
}

/// Integer modulo by zero is NULL under every policy; it warns only under
/// [`DivisionByZero::NullWithWarning`].
pub(crate) fn modulo_by_zero() {
    if policy().division_by_zero == DivisionByZero::NullWithWarning {
        push(Diagnostic::division_by_zero());
    }
}

/// Floating-point division by zero: true means "evaluate to NULL" (warning recorded); false
/// keeps IEEE semantics (the default policy).
pub(crate) fn float_division_by_zero() -> bool {
    if policy().division_by_zero == DivisionByZero::NullWithWarning {
        push(Diagnostic::division_by_zero());
        true
    } else {
        false
    }
}

/// Lenient numeric conversion of `s` when the policy allows it: the number text to parse
/// instead, and whether anything was dropped (a warning is due). `None`: no lenient
/// conversion, report the original parse error.
pub(crate) fn numeric_prefix(s: &str, target: NumericTarget) -> Option<(String, bool)> {
    if !policy().lenient_numeric_strings {
        return None;
    }
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c');
    let t = s.trim_start_matches(is_space);
    let b = t.as_bytes();
    let mut i = 0;
    let negative = matches!(b.first(), Some(b'-'));
    if matches!(b.first(), Some(b'-' | b'+')) {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i > int_start;
    let mut end = i;
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > i + 1 || digits {
            if target == NumericTarget::Integer {
                return None;
            }
            digits |= j > i + 1;
            end = j;
        }
    }
    if digits && end < b.len() && matches!(b[end], b'e' | b'E') {
        let mut j = end + 1;
        if j < b.len() && matches!(b[j], b'-' | b'+') {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            if target == NumericTarget::Integer {
                return None;
            }
            end = j;
        }
    }
    let rest = &t[end..];
    let warn = !digits || !rest.trim_matches(is_space).is_empty();
    let number = if digits {
        let n = &t[int_start..end];
        let n = n.strip_suffix('.').unwrap_or(n);
        let n = if n.starts_with('.') {
            format!("0{n}")
        } else {
            n.to_string()
        };
        if negative {
            format!("-{n}")
        } else {
            n
        }
    } else {
        "0".to_string()
    };
    Some((number, warn))
}

/// Casts `value` for storing into `column` (row `row`, 1-based) of type `ty`: never lenient
/// about malformed numeric strings, and a [`DiagnosticKind::DataTruncated`] note when DECIMAL
/// scale rounds digits away.
pub(crate) fn store_cast(
    value: DataValue,
    ty: &crate::types::LogicalType,
    column: &str,
    row: usize,
) -> Result<DataValue, crate::errors::DatabaseError> {
    let lost_digits = drops_decimal_digits(&value, ty);
    let value = strict_store(|| value.cast(ty))?;
    if lost_digits {
        push(Diagnostic::data_truncated(column, row));
    }
    Ok(value)
}

#[cfg(not(feature = "decimal"))]
pub(crate) fn drops_decimal_digits(_: &DataValue, _: &crate::types::LogicalType) -> bool {
    false
}

/// True when storing `value` into a DECIMAL(p, s) column rounds away non-zero fractional
/// digits (the stored value differs from the one written).
#[cfg(feature = "decimal")]
pub(crate) fn drops_decimal_digits(value: &DataValue, ty: &crate::types::LogicalType) -> bool {
    use std::str::FromStr;
    let crate::types::LogicalType::Decimal(_, Some(scale)) = ty else {
        return false;
    };
    let exact = match value {
        DataValue::Decimal(d) => Some(*d),
        // shortest round-trip text, not the binary expansion
        DataValue::Float64(f) => rust_decimal::Decimal::from_str(&f.0.to_string()).ok(),
        DataValue::Float32(f) => rust_decimal::Decimal::from_str(&f.0.to_string()).ok(),
        DataValue::Utf8 { value, .. } => rust_decimal::Decimal::from_str(value.trim()).ok(),
        _ => None,
    };
    exact.is_some_and(|d| d.normalize().scale() > *scale as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_is_statement_scoped() {
        begin_statement(StatementPolicy::default());
        push(Diagnostic::division_by_zero());
        assert_eq!(count(), 1);
        begin_statement(StatementPolicy::default());
        assert!(take().is_empty());
        push(Diagnostic::data_truncated("a", 2));
        let d = take();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].level, DiagnosticLevel::Note);
        assert_eq!(d[0].code, 0);
        assert_eq!(d[0].message, "data truncated for column 'a' at row 2");
        assert!(take().is_empty());
    }

    #[test]
    fn default_policy_keeps_errors() {
        begin_statement(StatementPolicy::default());
        assert!(division_by_zero().is_err());
        assert_eq!(numeric_prefix("12abc", NumericTarget::Integer), None);
        assert_eq!(count(), 0);
    }

    #[test]
    fn lenient_numeric_prefix() {
        begin_statement(StatementPolicy {
            lenient_numeric_strings: true,
            ..Default::default()
        });
        let p = |s: &str, t| numeric_prefix(s, t);
        assert_eq!(
            p("12abc", NumericTarget::Integer),
            Some(("12".into(), true))
        );
        assert_eq!(
            p("  -7  ", NumericTarget::Integer),
            Some(("-7".into(), false))
        );
        assert_eq!(p("a10", NumericTarget::Integer), Some(("0".into(), true)));
        assert_eq!(p("", NumericTarget::Integer), Some(("0".into(), true)));
        assert_eq!(p("1.5x", NumericTarget::Integer), None);
        assert_eq!(p("1e3", NumericTarget::Integer), None);
        assert_eq!(p("1.5x", NumericTarget::Double), Some(("1.5".into(), true)));
        assert_eq!(
            p("12e-3", NumericTarget::Decimal),
            Some(("12e-3".into(), false))
        );
        assert_eq!(p(".5", NumericTarget::Double), Some(("0.5".into(), false)));
        assert_eq!(p("2.", NumericTarget::Double), Some(("2".into(), false)));
        assert_eq!(p("1e", NumericTarget::Double), Some(("1".into(), true)));
        assert_eq!(p("+3", NumericTarget::Integer), Some(("3".into(), false)));
        strict_store(|| assert_eq!(p("12abc", NumericTarget::Integer), None));
        assert!(policy().lenient_numeric_strings);
    }
}
