# Statement-Level Atomicity Implementation Blocker

## Requirement
MySQL-style statement-level atomicity: when a statement fails mid-execution inside an explicit transaction, only that statement's partial writes should be rolled back, keeping the transaction open for subsequent statements.

## What's Implemented
- `Session<'db, S>` borrows Database and holds real `Transaction<'db, S>`
- BEGIN, COMMIT, ROLLBACK work correctly on LMDB
- SET autocommit = 0/1 with MySQL semantics  
- Transaction-level atomicity (ROLLBACK discards all changes)
- Statements that fail during planning before writes don't affect transaction

## Architectural Blocker

**The Core Issue**: To capture undo information for writes, we need to query prior values. But:
- `StorageTransaction::set(key, value)` and `remove(key)` are **synchronous** methods
- `StorageTransaction::get_borrowed(key)` is an **async** method
- You cannot `await` in a synchronous method

This prevents creating a transparent wrapper that intercepts writes and captures prior values.

## Attempted Approaches

### 1. Wrapper Transaction
```rust
struct UndoTrackingTransaction<T> {
    inner: T,
    undo_log: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl<T: StorageTransaction> StorageTransaction for UndoTrackingTransaction<T> {
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<(), DatabaseError> {
        // BLOCKED: Need to call self.inner.get_borrowed(key).await here
        // but set() is synchronous
    }
}
```

### 2. Pre-Execution Capture
Analyze the SQL statement, determine which keys will be affected, capture their values before execution, then restore on error.

**Blocked by**:
- Complex analysis required (WHERE clauses, joins, etc.)
- For `UPDATE test SET val = 100`, need to scan entire table first
- Performance implications of double-scanning
- Still requires coordinating async capture with sync execution

### 3. Write Buffer
Buffer all writes, flush on success, discard on error.

**Blocked by**:
- Can't intercept writes without wrapping StorageTransaction
- Same async/sync mismatch prevents transparent interception

### 4. Nested Transactions  
Execute each statement in a sub-transaction with savepoint semantics.

**Blocked by**:
- LMDB doesn't support nested write transactions
- MemoryStorage doesn't support savepoints
- Would require StorageTransaction API changes

## What Would Enable This

One of:
1. **Make StorageTransaction::set/remove async** - allows capturing prior values
2. **Add synchronous get** to StorageTransaction - allows synchronous undo capture
3. **Add savepoint/nested transaction support** - execute statements in rollbackable contexts
4. **Restructure executor** - buffer all writes at executor level before flushing to storage

All of these require changes beyond "don't modify StorageTransaction trait".

## Test Status

Tests for statement atomicity are in place but marked `#[ignore]`:
- `test_statement_atomicity_insert`: Multi-row INSERT failing on row 2
- `test_statement_atomicity_update`: UPDATE failing partway

These demonstrate the expected MySQL behavior and can be enabled once the blocker is resolved.
