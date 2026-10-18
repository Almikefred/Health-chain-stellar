//! Tests for the temperature contract.
//!
//! These tests exercise the oracle-to-unit assignment enforcement added for
//! issue #1648: a whitelisted oracle may only log readings for units it has
//! been explicitly assigned to, preventing a single compromised sensor key
//! from masking excursions or fabricating them across the fleet.

use super::*;
use soroban_sdk::{testutils::Address as _, Address, Env};

fn setup() -> (Env, TemperatureContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, TemperatureContract);
    let client = TemperatureContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);
    let oracle = Address::generate(&env);
    client.add_oracle(&admin, &oracle);
    (env, client, admin, oracle)
}

#[test]
fn assigned_oracle_can_log_reading() {
    let (env, client, admin, oracle) = setup();
    let unit_id = 1u64;
    client.assign_oracle(&admin, &unit_id, &oracle);

    client.log_reading(&oracle, &unit_id, &400);
    let reading = client.get_latest_reading(&unit_id).unwrap();
    assert_eq!(reading.temperature_celsius_x100, 400);
}

#[test]
fn unassigned_oracle_cannot_log_reading() {
    let (_env, client, _admin, oracle) = setup();
    let unit_id = 7u64;

    let result = client.try_log_reading(&oracle, &unit_id, &400);
    assert_eq!(result, Err(Ok(ContractError::OracleNotAssignedToUnit)));
}

#[test]
fn oracle_assigned_to_other_unit_cannot_log() {
    let (_env, client, admin, oracle) = setup();
    client.assign_oracle(&admin, &1u64, &oracle);

    let result = client.try_log_reading(&oracle, &2u64, &400);
    assert_eq!(result, Err(Ok(ContractError::OracleNotAssignedToUnit)));
}

#[test]
fn unassigned_oracle_cannot_mask_excursion() {
    let (env, client, admin, oracle) = setup();
    let unit_id = 42u64;
    client.assign_oracle(&admin, &unit_id, &oracle);

    // Two genuine violations from the assigned oracle.
    client.log_reading(&oracle, &unit_id, &900);
    client.log_reading(&oracle, &unit_id, &900);

    // A different whitelisted but unassigned oracle attempts to inject an
    // in-range reading to reset the streak.
    let rogue = Address::generate(&env);
    client.add_oracle(&admin, &rogue);
    let result = client.try_log_reading(&rogue, &unit_id, &400);
    assert_eq!(result, Err(Ok(ContractError::OracleNotAssignedToUnit)));

    // Streak must remain intact; a third violation compromises the unit.
    client.log_reading(&oracle, &unit_id, &900);
    assert!(client.is_compromised(&unit_id));
}

#[test]
fn unassigned_oracle_cannot_fabricate_excursion() {
    let (env, client, admin, oracle) = setup();
    let unit_id = 99u64;
    client.assign_oracle(&admin, &unit_id, &oracle);

    let rogue = Address::generate(&env);
    client.add_oracle(&admin, &rogue);

    for _ in 0..3 {
        let result = client.try_log_reading(&rogue, &unit_id, &900);
        assert_eq!(result, Err(Ok(ContractError::OracleNotAssignedToUnit)));
    }
    assert!(!client.is_compromised(&unit_id));
}

#[test]
fn revoked_oracle_cannot_log_reading() {
    let (_env, client, admin, oracle) = setup();
    let unit_id = 5u64;
    client.assign_oracle(&admin, &unit_id, &oracle);
    client.revoke_oracle(&admin, &unit_id, &oracle);

    let result = client.try_log_reading(&oracle, &unit_id, &400);
    assert_eq!(result, Err(Ok(ContractError::OracleNotAssignedToUnit)));
}
