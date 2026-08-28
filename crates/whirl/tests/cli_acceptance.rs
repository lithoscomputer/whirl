//! CLI acceptance tests (CLI acceptance-tests decision): every
//! acceptance test that needs only stdout, stderr, and the exit status
//! is a `trycmd` case under `tests/cmd/`. Each case runs the `whirl`
//! binary in a sandbox copy of its `.in/` fixture directory, so `fmt`
//! rewrites happen on copies.

#[test]
fn cli_cases() {
    trycmd::TestCases::new().case("tests/cmd/*.toml");
}
