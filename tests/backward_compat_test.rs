//! Backward compatibility test with LMDB fixture from main (fc6ca71).
//!
//! This test verifies that:
//! 1. Databases created by main can be opened by this build
//! 2. Tables, columns, and indexes can be accessed
//! 3. Data can be queried and index lookups work
//! 4. No corruption or incompatibility issues

#[cfg(all(feature = "lmdb", feature = "parser"))]
mod backward_compat {
    use kite_sql::db::DataBaseBuilder;
    use kite_sql::errors::DatabaseError;
    use kite_sql::types::value::DataValue;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_main_fc6ca71_database_compatibility() -> Result<(), DatabaseError> {
        // Copy fixture to temp directory so we don't modify the original
        let temp_dir = TempDir::new()?;
        let test_db_path = temp_dir.path().join("test_db");
        
        let fixture_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/main_fc6ca71_db"
        );
        
        fs::create_dir_all(&test_db_path)?;
        for entry in fs::read_dir(fixture_path)? {
            let entry = entry?;
            let dest = test_db_path.join(entry.file_name());
            fs::copy(entry.path(), dest)?;
        }

        // Open the database created by main - this is the key compatibility test
        let mut db = DataBaseBuilder::path(test_db_path.to_str().unwrap()).build_lmdb()?;

        // Verify we can query the users table
        let mut result = db.run("SELECT * FROM users ORDER BY id")?;
        
        let row1 = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(row1[0], DataValue::Int32(Some(1)));
        assert_eq!(row1[1], DataValue::from("Alice".to_string()));
        assert_eq!(row1[2], DataValue::Int32(Some(30)));
        
        let row2 = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(row2[0], DataValue::Int32(Some(2)));
        assert_eq!(row2[1], DataValue::from("Bob".to_string()));
        
        let row3 = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(row3[0], DataValue::Int32(Some(3)));
        assert_eq!(row3[1], DataValue::from("Charlie".to_string()));
        
        drop(result);

        // Verify unique index lookup works
        let mut result = db.run("SELECT id, age FROM users WHERE name = 'Alice'")?;
        let row = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(row[0], DataValue::Int32(Some(1)));
        assert_eq!(row[1], DataValue::Int32(Some(30)));
        drop(result);

        // Verify orders table
        let mut result = db.run("SELECT * FROM orders ORDER BY order_id")?;
        let order1 = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(order1[0], DataValue::Int32(Some(100)));
        assert_eq!(order1[1], DataValue::Int32(Some(1)));
        
        let order2 = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(order2[0], DataValue::Int32(Some(101)));
        assert_eq!(order2[1], DataValue::Int32(Some(2)));
        drop(result);

        // Verify DESCRIBE works (column metadata intact)
        let mut result = db.run("DESCRIBE users")?;
        
        let id_col = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(id_col[0], DataValue::from("id".to_string()));
        // Generic dialect, no display type
        assert_eq!(id_col[1], DataValue::from("Integer".to_string()));
        
        let name_col = result.next_tuple(|_, row| row.clone())?.unwrap();
        assert_eq!(name_col[0], DataValue::from("name".to_string()));
        assert_eq!(name_col[1], DataValue::from("Varchar(Some(50), CHARACTERS)".to_string()));
        
        drop(result);

        Ok(())
    }
}
