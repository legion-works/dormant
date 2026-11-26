//! TOML values for integration test fixtures.

/// Encode a TOML basic string without treating path backslashes as escapes.
pub fn toml_string(value: &str) -> String {
    // JSON string escapes also work in TOML. JSON leaves DEL unescaped,
    // but TOML does not permit a raw DEL in a basic string.
    assert!(
        !value.contains('\u{7f}'),
        "fixture value must not contain DEL"
    );
    serde_json::to_string(value).expect("encode fixture string")
}
