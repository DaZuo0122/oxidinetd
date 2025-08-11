# Testing oxidinetd

This project includes unit tests to verify the functionality of the cross-protocol forwarding features.

## Running Tests

To run all tests, use:

```bash
cargo test
```

This will run both the unit tests in the source files and the integration tests in the `tests/` directory.

## Test Coverage

The tests cover:

1. **Configuration parsing** - Verifies that the new protocol types (`udptotcp` and `tcptoudp`) are correctly parsed
2. **Enum serialization** - Ensures the Protocol enum works correctly with serde
3. **Default values** - Confirms that the default protocol is still TCP
4. **Mixed protocols** - Tests configurations with multiple different protocol types

## Adding New Tests

To add new tests, you can either:

1. Add unit tests directly in the source files (next to the code being tested)
2. Add integration tests in the `tests/` directory

All tests should follow Rust testing conventions and use the standard `#[test]` attribute.