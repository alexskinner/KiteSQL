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

//! SQL dialect abstraction for KiteSQL.
//!
//! This module provides a registry of SQL dialects that can be used with KiteSQL.
//! Each dialect maps to a sqlparser dialect and can customize parsing behavior.

use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect, MySqlDialect};
use std::fmt;

/// SQL dialect enum for KiteSQL.
///
/// Determines which SQL dialect to use for parsing and planning.
/// The default is `Generic`, which provides maximum compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    /// Generic SQL dialect (default) - maximum compatibility
    #[default]
    Generic,
    /// MySQL-compatible SQL dialect
    MySql,
}

impl Dialect {
    /// Get the sqlparser dialect instance for this KiteSQL dialect.
    pub(crate) fn sqlparser_dialect(&self) -> Box<dyn SqlParserDialect> {
        match self {
            Dialect::Generic => Box::new(GenericDialect {}),
            Dialect::MySql => Box::new(MySqlDialect {}),
        }
    }

    /// Get a static string name for this dialect (for cache keys).
    #[allow(dead_code)]
    pub(crate) fn cache_key(&self) -> &'static str {
        match self {
            Dialect::Generic => "generic",
            Dialect::MySql => "mysql",
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dialect::Generic => write!(f, "Generic"),
            Dialect::MySql => write!(f, "MySQL"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_dialect() {
        assert_eq!(Dialect::default(), Dialect::Generic);
    }

    #[test]
    fn test_cache_keys_are_unique() {
        assert_ne!(Dialect::Generic.cache_key(), Dialect::MySql.cache_key());
    }

    #[test]
    fn test_dialect_display() {
        assert_eq!(Dialect::Generic.to_string(), "Generic");
        assert_eq!(Dialect::MySql.to_string(), "MySQL");
    }
}
