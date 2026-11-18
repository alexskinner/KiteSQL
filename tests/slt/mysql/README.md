# MySQL Dialect Test Corpus

This directory contains SQL Logic Tests specifically for MySQL dialect features.

## Current Status

The tests in this directory are **future infrastructure** for MySQL dialect support. They currently:
- Run successfully using KiteSQL's Generic dialect
- Test MySQL-compatible syntax (backtick identifiers, etc.)
- Serve as a corpus for future MySQL-specific optimizations

## Future Work

When full MySQL dialect support is implemented:
1. Tests will run with `Dialect::MySql` explicitly set
2. MySQL-specific behaviors (date functions, string handling, etc.) will be validated
3. Dialect directive support may be added to the sqllogictest harness

## Test Organization

- `basic.slt` - Basic MySQL syntax features (backtick identifiers, multi-row INSERT)
- Additional tests can be added as MySQL-specific features are implemented

## Running Tests

```bash
# Run all MySQL tests
cd tests/sqllogictest
cargo run -- --path "../../tests/slt/mysql/*.slt"

# Run full suite (includes MySQL tests)
cargo run
```

## See Also

- [W1 PR](https://github.com/alexskinner/KiteSQL/pull/14) - Dialect registry implementation
- [Dialect enum](../../src/dialect.rs) - Dialect abstraction
