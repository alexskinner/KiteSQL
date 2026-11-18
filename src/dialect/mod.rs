use crate::errors::DatabaseError;
use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect as SqlParserDialect, GenericDialect, MySqlDialect};
use std::fmt::Debug;
use std::sync::Arc;

pub const DIALECT_GENERIC: &str = "generic";
pub const DIALECT_MYSQL: &str = "mysql";

pub trait SqlDialect: Debug + Send + Sync + 'static {
    fn name(&self) -> &str;

    fn parser_dialect(&self) -> &dyn SqlParserDialect;

    fn rewrite(&self, _stmt: &mut Statement) -> Result<(), DatabaseError> {
        Ok(())
    }

    fn supports_mysql_types(&self) -> bool {
        false
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

#[derive(Debug)]
pub struct MySqlSqlDialect {
    parser: MySqlDialect,
}

impl MySqlSqlDialect {
    pub fn new() -> Self {
        Self {
            parser: MySqlDialect {},
        }
    }
}

impl Default for MySqlSqlDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlDialect for MySqlSqlDialect {
    fn name(&self) -> &str {
        DIALECT_MYSQL
    }

    fn parser_dialect(&self) -> &dyn SqlParserDialect {
        &self.parser
    }

    fn supports_mysql_types(&self) -> bool {
        true
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

        let mysql = Arc::new(MySqlSqlDialect::new()) as Arc<dyn SqlDialect>;
        dialects.insert(DIALECT_MYSQL.to_string(), mysql);

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
