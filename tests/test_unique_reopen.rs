#![cfg(all(not(feature = "async"), feature = "lmdb"))]

use kite_sql::db::{CatalogKind, DataBaseBuilder};
use kite_sql::errors::DatabaseError;
use tempfile::TempDir;

#[test]
fn test_separate_unique_index_reopen() -> Result<(), DatabaseError> {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;
        database.ddl("CREATE TABLE t (id INT PRIMARY KEY, email VARCHAR(100))")?;
        database.ddl("CREATE UNIQUE INDEX uk_email ON t (email)")?;
        database
            .run("INSERT INTO t VALUES (1, 'test@example.com')")?
            .done()?;
    }

    {
        let mut database = DataBaseBuilder::path(db_path).build_lmdb()?;
        // Try without .load() first
        let result = database.run("INSERT INTO t VALUES (2, 'test@example.com')");
        assert!(
            result.is_err(),
            "Separate UNIQUE INDEX should be enforced after reopen (without load)"
        );

        Ok(())
    }
}
