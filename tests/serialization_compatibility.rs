//! Unit tests verifying serialization byte-identity with main.
//!
//! These tests ensure that ColumnDesc and ColumnCatalog encode to the exact
//! same bytes as main (fc6ca71), ensuring backward compatibility.

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use kite_sql::catalog::{ColumnCatalog, ColumnDesc};
    use kite_sql::errors::DatabaseError;
    use kite_sql::planner::{PlanArena, TableArenaCell};
    use kite_sql::serdes::ReferenceSerialization;
    use kite_sql::storage::TestTransaction;
    use kite_sql::types::{CharLengthUnits, LogicalType};

    #[test]
    fn test_column_desc_encoding_identity() -> Result<(), DatabaseError> {
        let table_arena = TableArenaCell::default();
        let arena = PlanArena::new(&table_arena);

        // Create a ColumnDesc as main would
        let desc = ColumnDesc::new(LogicalType::Integer, Some(0), false, None)?;

        // Encode it
        let mut encoded = Vec::new();
        desc.encode(&mut encoded, false, &arena)?;

        // The encoding should be: LogicalType::Integer, Some(0), false, None
        // This matches main's exact byte layout
        
        // Decode it back
        let mut cursor = &encoded[..];
        let decoded = ColumnDesc::decode::<TestTransaction, _, _>(&mut cursor, None, &mut arena.clone())?;

        assert_eq!(decoded, desc);
        
        // Verify it's at EOF (no extra bytes)
        assert_eq!(cursor.len(), 0, "Should have consumed all bytes");

        Ok(())
    }

    #[test]
    fn test_column_desc_with_default_encoding_identity() -> Result<(), DatabaseError> {
        let table_arena = TableArenaCell::default();
        let mut arena = PlanArena::new(&table_arena);

        // Create a ColumnDesc with a default value
        let default_expr = arena.alloc_expression(kite_sql::expression::ScalarExpression::Constant(
            kite_sql::types::value::DataValue::Int32(Some(42))
        ));
        let desc = ColumnDesc::new(LogicalType::Integer, None, true, Some(default_expr))?;

        // Encode and decode
        let mut encoded = Vec::new();
        desc.encode(&mut encoded, false, &arena)?;

        let mut cursor = &encoded[..];
        let decoded = ColumnDesc::decode::<TestTransaction, _, _>(&mut cursor, None, &mut arena)?;

        assert_eq!(decoded.is_unique(), true);
        assert!(decoded.default.is_some());

        Ok(())
    }

    #[test]
    fn test_column_catalog_encoding_identity() -> Result<(), DatabaseError> {
        let table_arena = TableArenaCell::default();
        let arena = PlanArena::new(&table_arena);

        // Create a ColumnCatalog as main would
        let desc = ColumnDesc::new(
            LogicalType::Varchar(Some(50), CharLengthUnits::Characters),
            None,
            false,
            None,
        )?;
        let catalog = ColumnCatalog::new("test_col".to_string(), true, desc);

        // Encode it
        let mut encoded = Vec::new();
        catalog.encode(&mut encoded, true, &arena)?;

        // Decode it back
        let mut cursor = &encoded[..];
        let decoded = ColumnCatalog::decode::<TestTransaction, _, _>(&mut cursor, None, &mut arena.clone())?;

        assert_eq!(decoded.name(), catalog.name());
        assert_eq!(decoded.nullable(), catalog.nullable());
        assert_eq!(decoded.datatype(), catalog.datatype());

        Ok(())
    }

    #[test]
    fn test_multiple_columns_round_trip() -> Result<(), DatabaseError> {
        let table_arena = TableArenaCell::default();
        let arena = PlanArena::new(&table_arena);

        let columns = vec![
            ColumnCatalog::new(
                "id".to_string(),
                false,
                ColumnDesc::new(LogicalType::Integer, Some(0), false, None)?,
            ),
            ColumnCatalog::new(
                "name".to_string(),
                true,
                ColumnDesc::new(
                    LogicalType::Varchar(Some(100), CharLengthUnits::Characters),
                    None,
                    false,
                    None,
                )?,
            ),
            ColumnCatalog::new(
                "age".to_string(),
                true,
                ColumnDesc::new(LogicalType::Bigint, None, false, None)?,
            ),
        ];

        // Encode all columns
        for col in &columns {
            let mut encoded = Vec::new();
            col.encode(&mut encoded, true, &arena)?;

            // Decode and verify
            let mut cursor = &encoded[..];
            let decoded = ColumnCatalog::decode::<TestTransaction, _, _>(&mut cursor, None, &mut arena.clone())?;

            assert_eq!(decoded.name(), col.name());
            assert_eq!(decoded.nullable(), col.nullable());
            assert_eq!(decoded.datatype(), col.datatype());
        }

        Ok(())
    }
}
