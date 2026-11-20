pub mod state;

use crate::errors::DatabaseError;
use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect};
use std::fmt::Debug;
use std::sync::Arc;

pub const DIALECT_GENERIC: &str = "generic";

#[derive(Debug, Clone)]
pub struct ColumnTypeSpec {
    pub logical: crate::types::LogicalType,
    pub display: Option<Arc<str>>,
}

impl ColumnTypeSpec {
    pub fn new(logical: crate::types::LogicalType) -> Self {
        Self {
            logical,
            display: None,
        }
    }

    pub fn with_display(mut self, display: impl Into<Arc<str>>) -> Self {
        self.display = Some(display.into());
        self
    }
}

pub trait SqlDialect: Debug + Send + Sync + 'static {
    fn name(&self) -> &str;

    fn parser_dialect(&self) -> &dyn SqlParserDialect;

    fn rewrite(&self, _stmt: &mut Statement) -> Result<(), DatabaseError> {
        Ok(())
    }

    fn map_data_type(
        &self,
        _data_type: &sqlparser::ast::DataType,
    ) -> Option<Result<crate::types::LogicalType, DatabaseError>> {
        None
    }

    fn map_column_type(
        &self,
        data_type: &sqlparser::ast::DataType,
    ) -> Option<Result<ColumnTypeSpec, DatabaseError>> {
        self.map_data_type(data_type)
            .map(|result| result.map(ColumnTypeSpec::new))
    }

    /// Evaluation choices for one parsed statement (division by zero, lenient numeric
    /// strings). The default keeps every such case an error.
    fn statement_policy(&self, _stmt: &Statement) -> crate::diagnostics::StatementPolicy {
        crate::diagnostics::StatementPolicy::default()
    }

    /// Whether inserting 0 into an identity column generates the next value, as NULL does.
    /// The default stores the 0.
    fn identity_zero_generates(&self) -> bool {
        false
    }

    /// Result type of `AVG` over an argument of type `arg`; `None` keeps the engine's DOUBLE.
    /// Only DOUBLE or DECIMAL are honoured: an AVG typed DECIMAL(p, s) is summed exactly and
    /// its quotient rounded half away from zero to scale s (unrounded when s is unset).
    fn avg_return_type(
        &self,
        _arg: &crate::types::LogicalType,
    ) -> Option<crate::types::LogicalType> {
        None
    }

    /// Value and type of a numeric literal such as `0.1`, `1e2` or `18446744073709551616`,
    /// given its text (with a leading `-` when the binder folded a unary minus into it);
    /// `None` keeps the engine's reading (INTEGER or BIGINT when it fits, otherwise DOUBLE).
    /// A returned type other than the value's own (for example a DECIMAL with the literal's
    /// precision and scale) is kept as the literal's type while binding.
    fn number_literal(
        &self,
        _text: &str,
    ) -> Option<Result<(crate::types::value::DataValue, crate::types::LogicalType), DatabaseError>>
    {
        None
    }

    /// Result type of the arithmetic `left <op> right` (`+`, `-`, `*`, `/` or `%`) for
    /// operands of these types; `None` keeps the engine's type. When the result is DECIMAL
    /// the operation is computed exactly in DECIMAL (integer operands included) and, when
    /// the type has a scale, the result is rounded half away from zero to that scale.
    fn arithmetic_return_type(
        &self,
        _op: &crate::expression::BinaryOperator,
        _left: &crate::types::LogicalType,
        _right: &crate::types::LogicalType,
    ) -> Option<crate::types::LogicalType> {
        None
    }

    /// The collation text compares by in this dialect's statements (see
    /// [`crate::types::collation`]); `None` (the default) compares byte-wise. Tables created
    /// while it is active store its keys in their primary key and indexes and are marked with
    /// its name, so a database keeps one collation for its life.
    fn text_collation(&self) -> Option<crate::types::collation::TextCollation> {
        None
    }

    /// Whether a column declared `COLLATE name` keeps byte-wise comparisons under
    /// [`Self::text_collation`] (a binary or case-sensitive collation). The default: none.
    fn is_binary_collation(&self, _name: &str) -> bool {
        false
    }

    /// Whether NULLs sort before every other value for a sort key that has no explicit
    /// `NULLS FIRST`/`NULLS LAST` (ORDER BY, window PARTITION BY/ORDER BY, GROUP_CONCAT
    /// ORDER BY); `asc` is the key's direction. The default sorts NULLs last in both
    /// directions, as an index scan returns them.
    fn nulls_first(&self, _asc: bool) -> bool {
        false
    }

    /// Maps a core diagnostic to this dialect's level, code and wording; `None` drops it.
    /// The default keeps it as raised (code 0).
    fn map_diagnostic(
        &self,
        diagnostic: crate::diagnostics::Diagnostic,
    ) -> Option<crate::diagnostics::Diagnostic> {
        Some(diagnostic)
    }

    /// The text representation of a boolean value when cast to CHAR or VARCHAR;
    /// `None` (the default) uses the boolean's string representation ('true'/'false').
    fn boolean_text(&self, _value: bool) -> Option<&'static str> {
        None
    }

    /// Result type for comparing `left` and `right` (`=`, `<>`, `<`, `>`, …); `None` (the
    /// default) keeps the engine's type. When this returns a type, the engine casts both
    /// operands to it before comparing.
    fn comparison_type(
        &self,
        _left: &crate::types::LogicalType,
        _right: &crate::types::LogicalType,
    ) -> Option<crate::types::LogicalType> {
        None
    }

    /// Whether this dialect supports `INSERT ... ON DUPLICATE KEY UPDATE` syntax.
    /// The default (`false`) rejects it.
    fn supports_on_duplicate_key_update(&self) -> bool {
        false
    }

    /// The affected-rows count for an upsert operation: (`inserted`, `updated`) → count.
    /// A dialect may return 2 for a changed row, 1 for an inserted row, 0 for an unchanged row.
    /// The default returns 1 for both insert and update (standard behavior).
    fn upsert_affected_rows(&self, _inserted: bool, _updated: bool) -> u64 {
        1
    }

    /// Whether BEGIN while a transaction is open commits the active transaction first.
    /// A dialect may return `true` (implicit commit), generic returns `false` (error).
    fn begin_commits_active_transaction(&self) -> bool {
        false
    }

    /// Whether this dialect supports `SET autocommit` syntax.
    /// The default is `false`.
    fn supports_set_autocommit(&self) -> bool {
        false
    }

    /// Whether this dialect supports prefix length in index key parts (`col(n)`).
    /// A dialect may accept and ignore the length. The default is `false`.
    fn supports_index_prefix_length(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct GenericSqlDialect {
    parser: GenericDialect,
}

impl GenericSqlDialect {
    pub fn new() -> Self {
        Self {
            parser: GenericDialect {},
        }
    }
}

impl Default for GenericSqlDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlDialect for GenericSqlDialect {
    fn name(&self) -> &str {
        DIALECT_GENERIC
    }

    fn parser_dialect(&self) -> &dyn SqlParserDialect {
        &self.parser
    }
}

#[derive(Debug, Clone)]
pub struct DialectRegistry {
    dialects: Arc<std::collections::HashMap<String, Arc<dyn SqlDialect>>>,
    default_name: String,
}

impl DialectRegistry {
    pub fn new() -> Self {
        let mut dialects = std::collections::HashMap::new();

        let generic = Arc::new(GenericSqlDialect::new()) as Arc<dyn SqlDialect>;
        dialects.insert(DIALECT_GENERIC.to_string(), generic.clone());

        Self {
            dialects: Arc::new(dialects),
            default_name: DIALECT_GENERIC.to_string(),
        }
    }

    pub fn register(&mut self, dialect: Arc<dyn SqlDialect>) {
        let name = dialect.name().to_string();
        Arc::make_mut(&mut self.dialects).insert(name, dialect);
    }

    pub fn set_default(&mut self, name: &str) -> Result<(), DatabaseError> {
        if !self.dialects.contains_key(name) {
            return Err(DatabaseError::UnsupportedStmt(format!(
                "unknown dialect: {}",
                name
            )));
        }
        self.default_name = name.to_string();
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn SqlDialect>, DatabaseError> {
        self.dialects
            .get(name)
            .cloned()
            .ok_or_else(|| DatabaseError::UnsupportedStmt(format!("unknown dialect: {}", name)))
    }

    pub fn default_dialect(&self) -> Arc<dyn SqlDialect> {
        self.dialects
            .get(&self.default_name)
            .cloned()
            .expect("default dialect not found")
    }
}

impl Default for DialectRegistry {
    fn default() -> Self {
        Self::new()
    }
}
