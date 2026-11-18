//! Generate LMDB fixture database for backward compatibility testing.
//! This should be run with main (fc6ca71) to create the fixture.

use kite_sql::db::DataBaseBuilder;
use kite_sql::errors::DatabaseError;

fn main() -> Result<(), DatabaseError> {
    let db_path = "/tmp/kite_fixture_db";
    
    // Remove any existing database
    let _ = std::fs::remove_dir_all(&db_path);
    
    // Create database
    let mut db = DataBaseBuilder::path(&db_path).build_lmdb()?;
    
    // Create a generic table with various types
    db.ddl(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name VARCHAR(50),
            age INTEGER,
            balance DECIMAL(10,2),
            active BOOLEAN
        )"
    )?;
    
    // Insert test data
    db.run("INSERT INTO users VALUES (1, 'Alice', 30, 1000.50, true)")?;
    db.run("INSERT INTO users VALUES (2, 'Bob', 25, 2500.75, false)")?;
    db.run("INSERT INTO users VALUES (3, 'Charlie', 35, 500.25, true)")?;
    
    // Create a unique index
    db.ddl("CREATE UNIQUE INDEX idx_name ON users(name)")?;
    
    // Create another table
    db.ddl(
        "CREATE TABLE orders (
            order_id INTEGER PRIMARY KEY,
            user_id INTEGER,
            amount DECIMAL(10,2)
        )"
    )?;
    
    db.run("INSERT INTO orders VALUES (100, 1, 99.99)")?;
    db.run("INSERT INTO orders VALUES (101, 2, 149.99)")?;
    
    println!("Fixture database created at: {}", db_path);
    println!("Files to commit:");
    for entry in std::fs::read_dir(&db_path)? {
        let entry = entry?;
        println!("  {}", entry.file_name().to_string_lossy());
    }
    
    Ok(())
}
