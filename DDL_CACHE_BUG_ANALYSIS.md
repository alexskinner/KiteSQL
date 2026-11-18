# DDL Catalog Cache Bug Analysis

## The Bug

When DDL (CREATE TABLE, ALTER TABLE, DROP TABLE) is executed through a `Session` within an explicit transaction:

1. **In-transaction**: The session's next statement in the same transaction fails with "table not found" because the shared catalog cache was never updated
2. **After COMMIT**: Running DESCRIBE or SELECT on the table from the Database fails until `db.load(CatalogKind::Table(...))` is manually called
3. **After ALTER**: The database's cached table catalog (including DisplayTypesMeta) remains stale until database reopen

**Root Cause**: `Session` executes statements through `Transaction::execute()`, which:
- Writes DDL changes to storage
- Commits the transaction
- Returns `TransactionIter` with pending `DDLApply` items
- **But never applies those changes to the shared cache**

Meanwhile, `Database::execute_mut()` (used by `db.ddl()` and `db.run()`) correctly extracts and applies DDL changes after commit.

## Why Plain `Database::run` Works

```rust
// Database::execute_mut() at line 1175-1259
1. Creates transaction
2. Executes statement → TransactionIter with DDLApply
3. Calls iter.done_with_ddl_apply() → extracts DDL changes
4. Commits transaction
5. Applies each DDLApply to shared caches:
   - apply_item.apply_to(&table_arena, &mut meta_cache, &mut plan_arena)
6. Calls recycle_table_arena() if catalog changed
```

## Why Session DDL Fails

```rust
// Session::execute_regular_statement_with_result() at line 226-253
1. Executes statement through transaction → TransactionIter
2. Returns SessionIter::Transaction(iter) to caller
3. Caller eventually calls done()
4. DDL changes are NEVER extracted or applied
5. Shared cache remains unchanged
```

## Architecture Constraints

### Shared Caches Use Interior Mutability

- `TableArenaCell`: wraps `TableArena` in `UnsafeCell` ✓ has interior mutability
- `StatisticsMetaCache`: plain struct with `HashMap` fields ✗ NO interior mutability

### Session Borrows Database

- `Session<'db, S>` borrows `&'db Database<S>`
- Multiple sessions can share one `Database` (important for worker thread pattern)
- Session CANNOT take `&mut Database` without breaking this

### BinderContext Uses Shared Cache

```rust
pub struct BinderContext<'a, T: StorageTransaction> {
    table_cache: &'a TableCache,  // from shared TableArena
    transaction: &'a T,
    // ...
}
```

When binding a statement:
1. Looks up table in `table_cache` (shared)
2. If not found, returns None → "table not found"
3. Does NOT fall back to loading from `transaction` storage

## Correct Solution

### Phase 1: Add Interior Mutability to StatisticsMetaCache

```rust
// In src/storage/mod.rs
pub(crate) struct StatisticsMetaCache {
    indexes: RwLock<HashMap<(TableId, IndexId), IndexStatistics>>,
    columns: RwLock<HashMap<(TableId, ColumnId), HistogramMeta>>,
}
```

Update all usage sites to acquire read/write locks.

### Phase 2: Extract and Apply DDL After Each Session Statement

In `Session`, after executing a statement in a transaction:

```rust
// Pseudo-code for Session::execute_regular_statement_with_result
if let Some(ref mut txn) = self.active_transaction {
    let mut result = BindSource::execute(txn, ...);
    
    // Extract DDL changes
    let (mut plan_arena, ddl_changes) = result.done_with_ddl_apply().await?;
    
    // Apply to shared cache immediately (for in-transaction visibility)
    for change in ddl_changes {
        // Track for rollback
        self.track_affected_table(&change);
        
        // Apply using interior mutability
        unsafe {
            change.apply_to(
                &database.state().table_arena,  // has interior mutability
                &database.state().meta_cache,    // now has interior mutability
                &mut plan_arena
            )
        }
    }
    
    // recycle if needed
    
    Ok(SessionIter::Empty)  // results already consumed above
}
```

### Phase 3: Handle Visibility Rules

**Within Transaction**:
- Apply DDL changes to shared cache immediately
- Session's next statements see them (reads shared cache)
- Other sessions DON'T see them (transaction isolation prevents seeing uncommitted data in storage)

**On COMMIT**:
- Changes already in shared cache ✓
- Clear `affected_tables` tracking

**On ROLLBACK**:
- For each affected table: reload from database storage (not transaction)
- This overwrites uncommitted versions with committed versions
- For tables that don't exist in storage: remove from cache

### Phase 4: Fix tests/fixtures/README.md

The README refers to `cargo run --example generate_fixture` but that example doesn't exist. Replace the build command with:

```bash
# Save the generation code to a temporary file and build as a scratch crate:
cat > /tmp/gen_fixture.rs << 'EOF'
use kite_sql::db::DataBaseBuilder;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ... (the code from the README)
}
EOF

rustc --edition 2021 /tmp/gen_fixture.rs \
  --extern kite_sql=/path/to/target/release/libkite_sql.rlib \
  -L dependency=/path/to/target/release/deps \
  -o /tmp/gen_fixture

/tmp/gen_fixture <output_dir>
```

Or better: document building it as an inline script against main SHA 5828c7c.

## Statement Atomicity (PR #26) Compatibility

PR #26 adds undo logs for statement-level rollback. When a DML statement fails mid-execution:
1. Undo log replays to roll back partial writes
2. Transaction stays open

For DDL statements that fail:
1. Undo log replays (if DDL writes were recorded)
2. Need to also undo cache updates

Solution: Track DDL changes separately from DML undo. If statement fails:
- Replay DML undo log
- Reload affected tables from transaction storage to revert cache

## Testing Requirements

Both generic (LMDB) and MySQL-specific (`with_mysql()`):

1. CREATE then INSERT then SELECT in transaction → COMMIT → SELECT via db (no manual load)
2. CREATE → ROLLBACK → table gone
3. ALTER ADD COLUMN → visible in session → COMMIT → visible in db with display types
4. ALTER → ROLLBACK → original schema restored
5. DROP → ROLLBACK → table still exists
6. Second session on same Database doesn't see first session's uncommitted CREATE
7. Failing statement after DDL in same transaction

## Files to Modify

1. `src/storage/mod.rs`: Add RwLock to StatisticsMetaCache
2. `src/db.rs`: Update apply_to signature, all access to meta_cache
3. `src/db/session.rs`: Extract and apply DDL after each statement
4. `tests/session_transaction_integration.rs`: Add all DDL visibility tests
5. `kite_sql_mysql/tests/`: Add MySQL-specific DDL tests
6. `tests/fixtures/README.md`: Fix non-existent example reference

## Implementation Complexity

**High** - Requires:
- Interior mutability refactor across codebase
- Careful handling of transaction visibility
- Integration with statement undo logs
- Comprehensive test coverage

**Estimated effort**: This is a significant architectural change affecting core catalog management.
