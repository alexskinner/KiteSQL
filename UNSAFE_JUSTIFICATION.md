# Why Session Cannot Be Fully Implemented Without Unsafe

## The Core Problem

The Session type needs to:
1. Own `Arc<Database<S>>` (for 'static lifetime and storage in structs)
2. Hold an open `Transaction<'_, S>` that borrows from that Database  
3. Keep the transaction alive across multiple SQL statements

This is a classic self-referential struct problem.

## Why Safe Crates Don't Work

Self-referential struct crates like `ouroboros`, `self_cell`, and `yoke` all have the same fundamental requirement: the borrowed type must be **covariant** over its lifetime parameter.

However, `Transaction<'a, S>` is **invariant** over `'a` because:
- It contains `inner: S::TransactionType<'a>`
- `StorageTransaction` types like LMDB's `RwTransaction<'a>` are invariant
- This is required for soundness in LMDB's single-writer guarantee

### Why Invariance Blocks Safe Solutions

Ouroboros and self_cell explicitly check for covariance at compile time and fail:
```rust
error: lifetime `'this` defined here
returning this value requires that `'this` must outlive `'static`
= note: the struct `Transaction<'a, S>` is invariant over the parameter `'a`
```

Yoke can handle invariant types, but requires implementing the `Yokeable` trait, which itself requires unsafe transmute operations.

## The Only Safe Alternative

Without modifying the Database/Transaction/Storage API itself, the only fully safe approach is:
- Track that Session is in "transaction mode"
- Execute statements through Database::run_statement() as normal
- Each statement gets its own underlying storage transaction

**This breaks ACID semantics** - statements aren't part of a single transaction, so:
- ROLLBACK can't undo previous statements
- Isolation doesn't work across statements
- The session becomes a state machine that doesn't actually control transactions

## What Would Need To Change

To make this work without unsafe, one of:

1. **Restructure Storage trait**: Make TransactionType covariant (likely impossible for LMDB safety)

2. **Restructure Database**: Add a `Database::create_session()` method that returns a type specifically designed to hold transactions, perhaps using GATs and different ownership patterns

3. **Accept the unsafe**: Use careful lifetime transmute with detailed safety documentation, as is common in database drivers (e.g., how tokio-postgres handles connection pools)

## Current Implementation

The current implementation tracks transaction state but doesn't keep actual transactions open, making it unsuitable for production use with LMDB.
