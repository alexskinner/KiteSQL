//! Creates an LMDB fixture database for backward compatibility testing.

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;

fn main() -> Result<(), DatabaseError> {
    let db_path = "/tmp/main_fixture_db";
    
    // Remove any existing database
    let _ = std::fs::remove_dir_all(db_path);
    
    // Create database
    let mut db = DataBaseBuilder::path(db_path).build_lmdb()?;
    
    // Create test_table with various types
    db.ddl(
        "CREATE TABLE test_table (
            id INT PRIMARY KEY,
            name VARCHAR(50),
            age INT,
            balance DECIMAL(10,2)
        )"
    )?;
    
    // Insert some test data
    db.run("INSERT INTO test_table VALUES (1, 'Alice', 30, 100.50)")?;
    db.run("INSERT INTO test_table VALUES (2, 'Bob', 25, 200.75)")?;
    db.run("INSERT INTO test_table VALUES (3, 'Charlie', 35, 300.25)")?;
    
    // Create unique index
    db.ddl("CREATE UNIQUE INDEX idx_name ON test_table(name)")?;
    
    // Create products table
    db.ddl(
        "CREATE TABLE products (
            product_id INT PRIMARY KEY,
            product_name VARCHAR(100),
            price DECIMAL(10,2)
        )"
    )?;
    
    // Insert product data
    db.run("INSERT INTO products VALUES (1, 'Widget', 9.99)")?;
    db.run("INSERT INTO products VALUES (2, 'Gadget', 19.99)")?;
    
    println!("Fixture database created at {}", db_path);
    println!("Copy files from {} to tests/fixtures/main_fixture_db/", db_path);
    
    Ok(())
}
