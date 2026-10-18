#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

const INSTANCE_TTL_THRESHOLD: u32 = 30 * 24 * 60 * 60 / 5; // ~30 days in ledgers
const INSTANCE_TTL_EXTEND_TO: u32 = 60 * 24 * 60 * 60 / 5; // ~60 days in ledgers

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    Paused,
    RequestContract,
    InventoryContract,
    PaymentContract,
}

#[contract]
pub struct CoordinatorContract;

fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

#[contractimpl]
impl CoordinatorContract {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        extend_instance_ttl(&env);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn propose_admin(env: Env, new_admin: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        extend_instance_ttl(&env);
    }

    pub fn accept_admin(env: Env) {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap();
        pending.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        extend_instance_ttl(&env);
    }

    pub fn pause(env: Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        extend_instance_ttl(&env);
    }

    pub fn unpause(env: Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        extend_instance_ttl(&env);
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn set_request_contract(env: Env, contract: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::RequestContract, &contract);
        extend_instance_ttl(&env);
    }

    pub fn set_inventory_contract(env: Env, contract: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::InventoryContract, &contract);
        extend_instance_ttl(&env);
    }

    pub fn set_payment_contract(env: Env, contract: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PaymentContract, &contract);
        extend_instance_ttl(&env);
    }

    pub fn get_request_contract(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::RequestContract)
            .unwrap()
    }

    pub fn get_inventory_contract(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .unwrap()
    }

    pub fn get_payment_contract(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .unwrap()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn initialize_extends_instance_ttl() {
        let env = Env::default();
        let contract_id = env.register_contract(None, CoordinatorContract);
        let client = CoordinatorContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(&admin);

        let ttl = env.storage().instance().get_ttl();
        assert!(ttl >= INSTANCE_TTL_THRESHOLD);
    }

    #[test]
    fn mutating_call_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, CoordinatorContract);
        let client = CoordinatorContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        client.initialize(&admin);
        client.pause();

        let ttl = env.storage().instance().get_ttl();
        assert!(ttl >= INSTANCE_TTL_THRESHOLD);
    }
}
