// Tests for FunctionRegistry overload resolution

use crate::errors::DatabaseError;
use crate::expression::function::FunctionSummary;
use crate::function::registry::FunctionRegistry;
use crate::types::LogicalType;
use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::abs::Abs;
    use crate::function::char_length::CharLength;

    #[test]
    fn test_resolve_scalar_exact_match() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs_i32 = Abs::new(LogicalType::Integer);
        let abs_f64 = Abs::new(LogicalType::Double);
        registry.register_scalar(abs_i32.clone())?;
        registry.register_scalar(abs_f64.clone())?;

        let result = registry.resolve_scalar("abs", &[LogicalType::Integer])?;
        assert_eq!(result.summary().arg_types, vec![LogicalType::Integer]);

        let result = registry.resolve_scalar("abs", &[LogicalType::Double])?;
        assert_eq!(result.summary().arg_types, vec![LogicalType::Double]);

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_widening_cast() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs_i64 = Abs::new(LogicalType::Bigint);
        registry.register_scalar(abs_i64)?;

        // Tinyint can implicitly cast to Bigint
        let result = registry.resolve_scalar("abs", &[LogicalType::Tinyint])?;
        assert_eq!(result.summary().arg_types, vec![LogicalType::Bigint]);

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_null_literal() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let char_len = CharLength::new("char_length".to_string());
        registry.register_scalar(char_len)?;

        // SqlNull should match any type (worst quality)
        let result = registry.resolve_scalar("char_length", &[LogicalType::SqlNull])?;
        assert_eq!(result.summary().name.as_ref(), "char_length");

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_prefers_exact_over_cast() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs_i32 = Abs::new(LogicalType::Integer);
        let abs_i64 = Abs::new(LogicalType::Bigint);
        registry.register_scalar(abs_i32.clone())?;
        registry.register_scalar(abs_i64)?;

        // Integer exact match should win over Tinyint->Bigint cast
        let result = registry.resolve_scalar("abs", &[LogicalType::Integer])?;
        assert_eq!(result.summary().arg_types, vec![LogicalType::Integer]);

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_not_found() {
        let registry = FunctionRegistry::new();
        
        let err = registry.resolve_scalar("nonexistent", &[LogicalType::Integer]);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn test_resolve_scalar_wrong_arg_count() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs_i32 = Abs::new(LogicalType::Integer);
        registry.register_scalar(abs_i32)?;

        // abs takes 1 arg, not 2
        let err = registry.resolve_scalar("abs", &[LogicalType::Integer, LogicalType::Integer]);
        assert!(err.is_err());

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_incompatible_types() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs_i32 = Abs::new(LogicalType::Integer);
        registry.register_scalar(abs_i32)?;

        // Boolean cannot cast to Integer
        let err = registry.resolve_scalar("abs", &[LogicalType::Boolean]);
        assert!(err.is_err());

        Ok(())
    }

    #[test]
    fn test_resolve_scalar_variadic() {
        let mut registry = FunctionRegistry::new();
        
        // Register a function that takes 1 argument
        let fn1 = CharLength::new("char_length".to_string());
        registry.register_scalar(fn1).unwrap();
        
        // Single argument: should succeed
        let arg_types = vec![LogicalType::Varchar(None, crate::types::CharLengthUnits::Characters)];
        assert!(registry.resolve_scalar("char_length", &arg_types).is_ok());
        
        // Two arguments: should fail (wrong arity)
        let arg_types_two = vec![
            LogicalType::Varchar(None, crate::types::CharLengthUnits::Characters),
            LogicalType::Varchar(None, crate::types::CharLengthUnits::Characters),
        ];
        let result = registry.resolve_scalar("char_length", &arg_types_two);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_scalar_ambiguity() {
        let mut registry = FunctionRegistry::new();
        
        // Register two overloads with same score potential
        let abs_int = Abs::new(LogicalType::Integer);
        registry.register_scalar(abs_int).unwrap();
        
        let abs_bigint = Abs::new(LogicalType::Bigint);
        registry.register_scalar(abs_bigint).unwrap();
        
        // Exact match: no ambiguity
        let arg_types_int = vec![LogicalType::Integer];
        let result = registry.resolve_scalar("abs", &arg_types_int);
        assert!(result.is_ok());
        let func = result.unwrap();
        assert_eq!(func.summary().arg_types, vec![LogicalType::Integer]);
        
        // With widening cast: Tinyint can cast to both Integer and Bigint with same score
        // The registry should pick the first registered one (Integer)
        let arg_types_tiny = vec![LogicalType::Tinyint];
        let result = registry.resolve_scalar("abs", &arg_types_tiny);
        assert!(result.is_ok());
        let func = result.unwrap();
        assert_eq!(func.summary().arg_types, vec![LogicalType::Integer]);
    }
}
