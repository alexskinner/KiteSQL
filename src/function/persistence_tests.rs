// Tests for function persistence and override semantics

#[cfg(test)]
mod tests {
    use crate::errors::DatabaseError;
    use crate::expression::function::scala::ScalarFunctionImpl;
    use crate::function::abs::Abs;
    use crate::function::registry::FunctionRegistry;
    use crate::types::LogicalType;

    #[test]
    fn test_register_scalar_rejects_duplicate() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs1 = Abs::new(LogicalType::Integer);
        let abs2 = Abs::new(LogicalType::Integer);

        registry.register_scalar(abs1)?;
        let err = registry.register_scalar(abs2);

        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("already exists"));

        Ok(())
    }

    #[test]
    fn test_register_with_options_allows_override() -> Result<(), DatabaseError> {
        let mut registry = FunctionRegistry::new();
        let abs1 = Abs::new(LogicalType::Integer);
        let abs2 = Abs::new(LogicalType::Integer);

        registry.register_scalar(abs1)?;
        // Should succeed with allow_override=true
        registry.register_scalar_with_options(abs2, true)?;

        Ok(())
    }

    #[test]
    fn test_persistence_policy_durable_by_default() -> Result<(), DatabaseError> {
        use crate::expression::function::PersistencePolicy;
        use crate::function::abs::Abs;

        let abs_fn = Abs::new(LogicalType::Integer);
        assert_eq!(abs_fn.summary().persistence, PersistencePolicy::Durable);

        Ok(())
    }

    #[test]
    fn test_load_functions_bypass_duplicate_check() -> Result<(), DatabaseError> {
        // load_scalar and load_table are intended for deserialization
        // and bypass the duplicate check (used during database reopen)
        let mut registry = FunctionRegistry::new();
        let abs1 = Abs::new(LogicalType::Integer);
        let abs2 = Abs::new(LogicalType::Integer);

        registry.load_scalar(abs1);
        registry.load_scalar(abs2); // Should not error, just replaces

        Ok(())
    }
}
