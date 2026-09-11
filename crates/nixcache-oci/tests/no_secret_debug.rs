#[test]
fn credential_holding_types_do_not_implement_debug() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/no_secret_debug.rs");
}
