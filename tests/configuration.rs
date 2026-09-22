#[path = "../crates/cc-config/tests/support/fixtures.rs"]
mod fixtures;

#[test]
fn shared_configuration_startup_contract() -> Result<(), Box<dyn std::error::Error>> {
    fixtures::cli_contract(env!("CARGO_BIN_EXE_rushls"), "RUSHLS_")
}
