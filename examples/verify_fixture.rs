//! Verifies the fixture database can be opened and queried.

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;

fn main() -> Result<(), DatabaseError> {
    let db_path = "/tmp/main_fixture_db";
    
    // Open database
    let mut db = DataBaseBuilder::path(db_path).build_lmdb()?;
    
    // Try to query test_table
    println!("Querying test_table...");
    db.run("SELECT * FROM test_table")?;
    println!("Query successful");
    
    // Try to query products
    println!("Querying products...");
    db.run("SELECT * FROM products")?;
    println!("Query successful");
    
    println!("Fixture verification successful!");
    
    Ok(())
}
