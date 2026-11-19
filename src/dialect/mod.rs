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

    /// Maps a core diagnostic to this dialect's level, code and wording; `None` drops it.
    /// The default keeps it as raised (code 0).
    fn map_diagnostic(
        &self,
        diagnostic: crate::diagnostics::Diagnostic,
    ) -> Option<crate::diagnostics::Diagnostic> {
        Some(diagnostic)
    }
}

#[derive(Debug)]
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
