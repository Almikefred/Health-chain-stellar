#![cfg(test)]

//! Coordinator tests.
//!
//! Most tests register lightweight mock implementations of the three domain
//! contracts (mirroring the coordinator's proxy structs exactly) so that
//! workflow state can be driven directly. `test_allocate_units_end_to_end_real_contracts`
//! additionally registers the *real* requests, inventory and payments contracts
//! and runs the whole allocate → confirm → settle sequence across them.

use soroban_sdk::{
    contract, contractimpl, contracttype, testutils::Address as _, testutils::Ledger as _,
    vec, Address, Env, Map, String,
};

use super::{
    BloodComponent, BloodRequest, BloodStatus, BloodType, BloodUnit, CoordinatorContract,
    CoordinatorContractClient, CoordinatorError, ExcursionSummary, Payment, PaymentStatus,
    RequestStatus, Urgency, WorkflowStatus,
};

use inventory_contract::{BloodStatus as InvBloodStatus, BloodType as InvBloodType};
use inventory_contract::{InventoryContract, InventoryContractClient};
use payment_contract::PaymentStatus as PayStatus;
use payment_contract::{PaymentContract, PaymentContractClient};
use request_contract::{BloodComponent as ReqComponent, BloodType as ReqBloodType};
use request_contract::{RequestContract, RequestContractClient, Urgency as ReqUrgency};

// ── Mock: Requests contract ───────────────────────────────────────────────────

#[contracttype]
enum ReqKey {
    Request(u64),
}

#[contract]
struct MockRequestContract;

#[contractimpl]
impl MockRequestContract {
    /// Test helper — seed a request with the given status and blood type.
    pub fn seed_request(
        env: Env,
        id: u64,
        hospital: Address,
        status: RequestStatus,
        blood_type: BloodType,
    ) {
        let request = BloodRequest {
            id,
            hospital_id: hospital,
            blood_type,
            component: BloodComponent::WholeBlood,
            quantity_ml: 450,
            urgency: Urgency::Routine,
            created_timestamp: 0,
            required_by_timestamp: 0,
            status,
            assigned_units: soroban_sdk::Vec::new(&env),
            fulfilled_quantity_ml: 0,
            reservation_id: None,
            fulfilled_by: None,
            history: soroban_sdk::Vec::new(&env),
        };
        env.storage().persistent().set(&ReqKey::Request(id), &request);
    }

    pub fn get_request(env: Env, request_id: u64) -> BloodRequest {
        env.storage()
            .persistent()
            .get(&ReqKey::Request(request_id))
            .expect("request not found")
    }
}

// ── Mock: Inventory contract ──────────────────────────────────────────────────

#[contracttype]
enum InvKey {
    Unit(u64),
    Counter,
}

#[contract]
struct MockInventoryContract;

#[contractimpl]
impl MockInventoryContract {
    /// Test helper — seed a unit with the given blood type and status.
    pub fn seed_unit(env: Env, blood_type: BloodType, status: BloodStatus) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&InvKey::Counter)
            .unwrap_or(0u64)
            + 1;
        env.storage().instance().set(&InvKey::Counter, &id);
        let unit = BloodUnit {
            id,
            blood_type,
            quantity_ml: 450,
            bank_id: env.current_contract_address(),
            donor_id: None,
            donation_timestamp: 0,
            expiration_timestamp: 10_000_000,
            status,
            metadata: Map::new(&env),
        };
        env.storage().persistent().set(&InvKey::Unit(id), &unit);
        id
    }

    pub fn get_blood_unit(env: Env, blood_unit_id: u64) -> BloodUnit {
        env.storage()
            .persistent()
            .get(&InvKey::Unit(blood_unit_id))
            .expect("unit not found")
    }

    pub fn update_status(
        env: Env,
        unit_id: u64,
        new_status: BloodStatus,
        _authorized_by: Address,
        _reason: Option<String>,
    ) -> BloodUnit {
        let mut unit = Self::get_blood_unit(env.clone(), unit_id);
        unit.status = new_status;
        env.storage()
            .persistent()
            .set(&InvKey::Unit(unit_id), &unit);
        unit
    }

    pub fn mark_delivered(
        env: Env,
        unit_id: u64,
        authorized_by: Address,
        delivery_location: String,
    ) -> BloodUnit {
        Self::update_status(
            env,
            unit_id,
            BloodStatus::Delivered,
            authorized_by,
            Some(delivery_location),
        )
    }
}

// ── Mock: Payment contract ────────────────────────────────────────────────────

#[contracttype]
enum PayKey {
    Payment(u64),
    Counter,
}

#[contract]
struct MockPaymentContract;

#[contractimpl]
impl MockPaymentContract {
    /// Test helper — seed a payment for `request_id` with the given status.
    pub fn seed_payment(env: Env, request_id: u64, status: PaymentStatus) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&PayKey::Counter)
            .unwrap_or(0u64)
            + 1;
        env.storage().instance().set(&PayKey::Counter, &id);
        let payment = Payment {
            id,
            request_id,
            payer: env.current_contract_address(),
            payee: env.current_contract_address(),
            amount: 1_000,
            status,
            created_at: 0,
            updated_at: 0,
            dispute_reason_code: None,
            dispute_case_id: None,
            dispute_resolved: false,
            token: None,
        };
        env.storage().persistent().set(&PayKey::Payment(id), &payment);
        id
    }

    pub fn get_payment(env: Env, payment_id: u64) -> Payment {
        env.storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .expect("payment not found")
    }

    pub fn update_status(env: Env, payment_id: u64, status: PaymentStatus, _caller: Address) {
        let mut payment = Self::get_payment(env.clone(), payment_id);
        payment.status = status;
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &payment);
    }

    /// Escrow-aware refund path (payments `refund_escrow`). Mirrors the real
    /// contract's precondition: only a Locked payment can be refunded.
    pub fn refund_escrow(env: Env, _caller: Address, payment_id: u64) {
        let mut p: Payment = env
            .storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .unwrap();
        if p.status == PaymentStatus::Locked {
            p.status = PaymentStatus::Refunded;
        }
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &p);
    }

    pub fn record_dispute(
        env: Env,
        payment_id: u64,
        _reason: super::payment_client::DisputeReason,
        case_id: String,
        _caller: Address,
    ) {
        let mut payment = Self::get_payment(env.clone(), payment_id);
        payment.status = PaymentStatus::Disputed;
        payment.dispute_case_id = Some(case_id);
        payment.dispute_resolved = false;
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &payment);
    }
}

// ── Harness ───────────────────────────────────────────────────────────────────

struct Harness<'a> {
    env: Env,
    admin: Address,
    coord: CoordinatorContractClient<'a>,
    req_id: Address,
    inv_id: Address,
    pay_id: Address,
}

fn setup<'a>() -> Harness<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    let req_id = env.register(MockRequestContract, ());
    let inv_id = env.register(MockInventoryContract, ());
    let pay_id = env.register(MockPaymentContract, ());
    let coord_id = env.register(CoordinatorContract, ());

    let coord = CoordinatorContractClient::new(&env, &coord_id);
    coord.initialize(&admin, &req_id, &inv_id, &pay_id);

    Harness {
        env,
        admin,
        coord,
        req_id,
        inv_id,
        pay_id,
    }
}

fn seed_request(h: &Harness, id: u64, status: RequestStatus) -> Address {
    let hospital = Address::generate(&h.env);
    MockRequestContractClient::new(&h.env, &h.req_id).seed_request(
        &id,
        &hospital,
        &status,
        &BloodType::OPositive,
    );
    hospital
}

fn seed_unit(h: &Harness, status: BloodStatus) -> u64 {
    MockInventoryContractClient::new(&h.env, &h.inv_id).seed_unit(&BloodType::OPositive, &status)
}

fn seed_payment(h: &Harness, request_id: u64, status: PaymentStatus) -> u64 {
    MockPaymentContractClient::new(&h.env, &h.pay_id).seed_payment(&request_id, &status)
}

fn get_unit(h: &Harness, unit_id: u64) -> BloodUnit {
    MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id)
}

fn get_payment(h: &Harness, payment_id: u64) -> Payment {
    MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id)
}

/// Allocate a single Available unit and a Locked payment for a Pending request.
fn allocate(h: &Harness, request_id: u64, unit_id: u64, payment_id: u64) {
    h.coord.allocate_units(
        &request_id,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
}

fn allocate_and_deliver(h: &Harness, request_id: u64, unit_id: u64, payment_id: u64) {
    allocate(h, request_id, unit_id, payment_id);
    h.coord.confirm_delivery(
        &request_id,
        &h.admin,
        &String::from_str(&h.env, "Hospital-A-GPS"),
    );
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[test]
fn test_full_happy_path() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);
    assert!(!wf.delivery_confirmed);
    assert_eq!(wf.payment_id, payment_id);
    assert_eq!(wf.unit_ids.get(0).unwrap(), unit_id);
    assert_eq!(get_unit(&h, unit_id).status, BloodStatus::Reserved);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Locked);

    h.coord.confirm_delivery(
        &1u64,
        &h.admin,
        &String::from_str(&h.env, "Hospital-A-GPS"),
    );

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Delivered);
    assert!(wf.delivery_confirmed);
    assert_eq!(
        wf.delivery_location,
        Some(String::from_str(&h.env, "Hospital-A-GPS"))
    );
    assert_eq!(get_unit(&h, unit_id).status, BloodStatus::Delivered);

    h.coord.settle_payment(&1u64, &h.admin);

    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Settled);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Released);
}

// ── Sequence enforcement ──────────────────────────────────────────────────────

#[test]
fn test_settle_blocked_without_delivery() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);

    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::DeliveryNotConfirmed)));
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Locked);
}

#[test]
fn test_settle_blocked_for_unlocked_payment() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Pending);

    allocate_and_deliver(&h, 1, unit_id, payment_id);

    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidPaymentState)));
}

#[test]
fn test_double_allocation_blocked() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);

    let unit_id2 = seed_unit(&h, BloodStatus::Available);
    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id2],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowAlreadyStarted)));
}

#[test]
fn test_allocate_blocked_for_unavailable_unit() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Reserved);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::UnitNotAvailable)));
}

#[test]
fn test_confirm_delivery_blocked_before_allocation() {
    let h = setup();
    let result = h.coord.try_confirm_delivery(
        &99u64,
        &h.admin,
        &String::from_str(&h.env, "Hospital-A-GPS"),
    );
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowNotFound)));
}

#[test]
fn test_allocate_blocked_for_non_pending_request() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Approved);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidRequestState)));
}

#[test]
fn test_allocate_units_rejects_empty_unit_ids() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::NoUnitsSpecified)));
    assert!(h.coord.try_get_workflow(&1u64).is_err());
}

#[test]
fn test_allocate_units_rejects_mismatched_payment() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    seed_request(&h, 2, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_for_request_2 = seed_payment(&h, 2, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_for_request_2,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::PaymentRequestMismatch)));
    assert!(h.coord.try_get_workflow(&1u64).is_err());
}

#[test]
fn test_allocate_units_rejects_incompatible_unit_blood_type() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    // AB- units cannot donate to an O+ recipient.
    let unit_id = MockInventoryContractClient::new(&h.env, &h.inv_id)
        .seed_unit(&BloodType::ABNegative, &BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::IncompatibleBloodType)));
}

// ── Rollback & expiry ─────────────────────────────────────────────────────────

#[test]
fn test_rollback_releases_units_and_refunds_payment() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);
    h.coord.rollback(&1u64);

    assert_eq!(
        h.coord.get_workflow(&1u64).status,
        WorkflowStatus::RolledBack
    );
    assert_eq!(get_unit(&h, unit_id).status, BloodStatus::Available);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Refunded);
}

#[test]
fn test_rollback_leaves_other_payment_states_untouched() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Pending);

    allocate(&h, 1, unit_id, payment_id);
    h.coord.rollback(&1u64);

    assert_eq!(get_unit(&h, unit_id).status, BloodStatus::Available);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Pending);
}

#[test]
fn test_rollback_blocked_after_settlement() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate_and_deliver(&h, 1, unit_id, payment_id);
    h.coord.settle_payment(&1u64, &h.admin);

    let result = h.coord.try_rollback(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::CannotRollbackSettled)));
}

#[test]
fn test_rollback_blocked_after_delivery() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate_and_deliver(&h, 1, unit_id, payment_id);

    let result = h.coord.try_rollback(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidWorkflowState)));
    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Delivered);
    assert_ne!(get_unit(&h, unit_id).status, BloodStatus::Available);
}

#[test]
fn test_allocate_units_allows_reallocation_after_rollback() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);
    h.coord.rollback(&1u64);

    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id2 = seed_unit(&h, BloodStatus::Available);
    let payment_id2 = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id2],
        &payment_id2,
        &h.admin,
        &BloodType::OPositive,
    );
    assert!(result.is_ok());

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);
    assert_eq!(wf.payment_id, payment_id2);
}

#[test]
fn test_expire_workflow_rolls_back_after_deadline() {
    let h = setup();
    h.env.ledger().with_mut(|l| l.timestamp = 1_000);

    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.expires_at, 1_000 + 6 * 60 * 60);

    h.env.ledger().with_mut(|l| l.timestamp = wf.expires_at + 1);
    h.coord.expire_workflow(&1u64);

    assert_eq!(
        h.coord.get_workflow(&1u64).status,
        WorkflowStatus::RolledBack
    );
    assert_eq!(get_unit(&h, unit_id).status, BloodStatus::Available);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Refunded);
}

#[test]
fn test_expire_workflow_blocked_before_deadline() {
    let h = setup();
    h.env.ledger().with_mut(|l| l.timestamp = 1_000);

    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    allocate(&h, 1, unit_id, payment_id);

    let result = h.coord.try_expire_workflow(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowNotExpired)));
    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Allocated);
}

#[test]
fn test_expire_workflow_not_found() {
    let h = setup();
    let result = h.coord.try_expire_workflow(&9999u64);
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowNotFound)));
}

// ── Pause circuit breaker ─────────────────────────────────────────────────────

#[test]
fn test_pause_blocks_allocate_units() {
    let h = setup();
    h.coord.pause(&h.admin);
    assert!(h.coord.is_paused());

    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::ContractPaused)));
}

#[test]
fn test_pause_allows_get_workflow_and_unpause_restores_writes() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate(&h, 1, unit_id, payment_id);

    h.coord.pause(&h.admin);
    assert_eq!(h.coord.get_workflow(&1u64).request_id, 1);

    h.coord.unpause(&h.admin);
    assert!(!h.coord.is_paused());

    h.coord.confirm_delivery(
        &1u64,
        &h.admin,
        &String::from_str(&h.env, "Hospital-A-GPS"),
    );
    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Delivered);
}

#[test]
fn test_rollback_blocked_when_paused() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate(&h, 1, unit_id, payment_id);

    h.coord.pause(&h.admin);
    let result = h.coord.try_rollback(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::ContractPaused)));
    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Allocated);
}

#[test]
fn test_non_admin_cannot_pause() {
    let h = setup();
    let attacker = Address::generate(&h.env);
    let result = h.coord.try_pause(&attacker);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
    assert!(!h.coord.is_paused());
}

// ── Emergency halt ────────────────────────────────────────────────────────────

#[test]
fn test_emergency_halt_blocks_workflow_steps() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    h.coord.emergency_halt(&h.admin);
    assert!(h.coord.is_emergency_halted());

    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::EmergencyHalted)));
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Locked);
}

#[test]
fn test_emergency_halt_blocks_new_allocations() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);

    h.coord.emergency_halt(&h.admin);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &h.admin,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::EmergencyHalted)));
}

#[test]
fn test_emergency_halt_non_admin_fails() {
    let h = setup();
    let attacker = Address::generate(&h.env);

    let result = h.coord.try_emergency_halt(&attacker);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
    assert!(!h.coord.is_emergency_halted());
}

#[test]
fn test_clear_emergency_halt_restores_operation() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    h.coord.emergency_halt(&h.admin);
    h.coord.clear_emergency_halt(&h.admin);
    assert!(!h.coord.is_emergency_halted());

    h.coord.settle_payment(&1u64, &h.admin);
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Released);
}

#[test]
fn test_clear_emergency_halt_non_admin_fails() {
    let h = setup();
    let attacker = Address::generate(&h.env);
    h.coord.emergency_halt(&h.admin);

    let result = h.coord.try_clear_emergency_halt(&attacker);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
    assert!(h.coord.is_emergency_halted());
}

#[test]
fn test_unpause_does_not_clear_emergency_halt() {
    let h = setup();
    h.coord.emergency_halt(&h.admin);
    h.coord.pause(&h.admin);
    h.coord.unpause(&h.admin);

    assert!(!h.coord.is_paused());
    assert!(
        h.coord.is_emergency_halted(),
        "unpause() must not silently clear an emergency halt"
    );

    let result = h.coord.try_expire_workflow(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::EmergencyHalted)));
}

// ── Role checks on workflow entry points ──────────────────────────────────────

#[test]
fn test_allocate_units_rejects_non_admin_caller() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    let attacker = Address::generate(&h.env);

    let result = h.coord.try_allocate_units(
        &1u64,
        &vec![&h.env, unit_id],
        &payment_id,
        &attacker,
        &BloodType::OPositive,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

#[test]
fn test_confirm_delivery_rejects_non_admin_caller() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate(&h, 1, unit_id, payment_id);

    let attacker = Address::generate(&h.env);
    let result = h.coord.try_confirm_delivery(
        &1u64,
        &attacker,
        &String::from_str(&h.env, "Fabricated-Location"),
    );
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

#[test]
fn test_settle_payment_rejects_non_admin_caller() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    let attacker = Address::generate(&h.env);
    let result = h.coord.try_settle_payment(&1u64, &attacker);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

// ── report_temperature_breach: ids must belong to the workflow ────────────────

#[test]
fn test_report_temperature_breach_disputes_bound_payment() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    h.coord
        .report_temperature_breach(&1u64, &unit_id, &payment_id);

    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Disputed);
}

#[test]
fn test_report_temperature_breach_rejects_unit_outside_workflow() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let allocated_unit = seed_unit(&h, BloodStatus::Available);
    let unrelated_unit = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, allocated_unit, payment_id);

    let result = h
        .coord
        .try_report_temperature_breach(&1u64, &unrelated_unit, &payment_id);
    assert_eq!(result, Err(Ok(CoordinatorError::UnitNotFound)));
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Locked);
}

#[test]
fn test_report_temperature_breach_rejects_payment_of_other_request() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    seed_request(&h, 2, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    let other_requests_payment = seed_payment(&h, 2, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    let result = h.coord.try_report_temperature_breach(
        &1u64,
        &unit_id,
        &other_requests_payment,
    );
    assert_eq!(result, Err(Ok(CoordinatorError::PaymentRequestMismatch)));
    assert_eq!(
        get_payment(&h, other_requests_payment).status,
        PaymentStatus::Locked,
        "an unrelated hospital's escrow must not be frozen"
    );
}

#[test]
fn test_report_temperature_breach_blocked_when_halted() {
    let h = setup();
    seed_request(&h, 1, RequestStatus::Pending);
    let unit_id = seed_unit(&h, BloodStatus::Available);
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    allocate_and_deliver(&h, 1, unit_id, payment_id);

    h.coord.emergency_halt(&h.admin);

    let result = h
        .coord
        .try_report_temperature_breach(&1u64, &unit_id, &payment_id);
    assert_eq!(result, Err(Ok(CoordinatorError::EmergencyHalted)));
}

// ── flag_temperature_breach (temperature oracle path) ─────────────────────────

fn make_excursion(unit_id: u64) -> ExcursionSummary {
    ExcursionSummary {
        unit_id,
        violation_count: 3,
        peak_celsius_x100: 1200,
        detected_at: 1000,
    }
}

#[test]
fn test_flag_temperature_breach_transitions_locked_to_disputed() {
    let h = setup();
    let payment_id = seed_payment(&h, 99, PaymentStatus::Locked);

    h.coord
        .flag_temperature_breach(&h.admin, &payment_id, &make_excursion(42));

    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Disputed);
}

#[test]
fn test_flag_temperature_breach_allows_configured_oracle() {
    let h = setup();
    let payment_id = seed_payment(&h, 99, PaymentStatus::Locked);
    let oracle = Address::generate(&h.env);
    h.coord.set_temperature_oracle(&h.admin, &oracle);

    h.coord
        .flag_temperature_breach(&oracle, &payment_id, &make_excursion(42));

    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Disputed);
}

#[test]
fn test_flag_temperature_breach_rejects_unconfigured_caller() {
    let h = setup();
    let payment_id = seed_payment(&h, 99, PaymentStatus::Locked);
    let attacker = Address::generate(&h.env);

    let result =
        h.coord
            .try_flag_temperature_breach(&attacker, &payment_id, &make_excursion(42));
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

#[test]
fn test_flag_temperature_breach_non_locked_payment_fails() {
    let h = setup();
    let payment_id = seed_payment(&h, 1, PaymentStatus::Released);

    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &payment_id, &make_excursion(1));
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidPaymentState)));
}

#[test]
fn test_flag_temperature_breach_missing_payment_fails() {
    let h = setup();
    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &9999u64, &make_excursion(1));
    assert_eq!(result, Err(Ok(CoordinatorError::PaymentNotFound)));
}

#[test]
fn test_flag_temperature_breach_blocked_when_paused() {
    let h = setup();
    let payment_id = seed_payment(&h, 1, PaymentStatus::Locked);
    h.coord.pause(&h.admin);

    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &payment_id, &make_excursion(1));
    assert_eq!(result, Err(Ok(CoordinatorError::ContractPaused)));
    assert_eq!(get_payment(&h, payment_id).status, PaymentStatus::Locked);
}

#[test]
fn test_set_temperature_oracle_rejects_non_admin_caller() {
    let h = setup();
    let attacker = Address::generate(&h.env);
    let oracle = Address::generate(&h.env);

    let result = h.coord.try_set_temperature_oracle(&attacker, &oracle);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

// ── Admin transfer ────────────────────────────────────────────────────────────

#[test]
fn test_propose_and_accept_admin_transfer() {
    let h = setup();
    let new_admin = Address::generate(&h.env);

    h.coord.propose_admin(&h.admin, &new_admin);
    h.coord.accept_admin(&new_admin);
}

#[test]
fn test_accept_admin_wrong_address_fails() {
    let h = setup();
    let new_admin = Address::generate(&h.env);
    let wrong_address = Address::generate(&h.env);

    h.coord.propose_admin(&h.admin, &new_admin);
    let result = h.coord.try_accept_admin(&wrong_address);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

#[test]
fn test_propose_admin_non_admin_fails() {
    let h = setup();
    let attacker = Address::generate(&h.env);
    let new_admin = Address::generate(&h.env);

    let result = h.coord.try_propose_admin(&attacker, &new_admin);
    assert_eq!(result, Err(Ok(CoordinatorError::Unauthorized)));
}

// ── End-to-end across the real requests / inventory / payments contracts ──────

/// Acceptance test for #1616: the coordinator mirrors the full `BloodRequest`,
/// `BloodUnit` and `Payment` structs, so the real contracts' return values
/// decode. This drives the whole workflow against the real domain contracts.
#[test]
fn test_allocate_units_end_to_end_real_contracts() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);

    let requests_id = env.register(RequestContract, ());
    let inventory_id = env.register(InventoryContract, ());
    let payments_id = env.register(PaymentContract, ());
    let coordinator_id = env.register(CoordinatorContract, ());

    let requests = RequestContractClient::new(&env, &requests_id);
    let inventory = InventoryContractClient::new(&env, &inventory_id);
    let payments = PaymentContractClient::new(&env, &payments_id);
    let coordinator = CoordinatorContractClient::new(&env, &coordinator_id);

    requests.initialize(&admin, &inventory_id);
    requests.authorize_hospital(&hospital);
    inventory.initialize(&admin);
    payments.initialize(&admin, &Some(requests_id.clone()));
    coordinator.initialize(&admin, &requests_id, &inventory_id, &payments_id);

    env.ledger().with_mut(|l| l.timestamp = 1_000);

    let request_id = requests.create_request(
        &hospital,
        &ReqBloodType::OPositive,
        &ReqComponent::WholeBlood,
        &250u32,
        &ReqUrgency::Routine,
        &2_000u64,
    );

    let unit_id = inventory.register_blood(
        &admin,
        &String::from_str(&env, "SERIAL-0001"),
        &InvBloodType::OPositive,
        &450u32,
        &None,
    );

    let payment_id = payments.create_payment(&request_id, &hospital, &payee, &500i128);
    payments.update_status(&payment_id, &PayStatus::Locked, &admin);

    // Step 1 — allocate.
    coordinator.allocate_units(
        &request_id,
        &vec![&env, unit_id],
        &payment_id,
        &admin,
        &BloodType::OPositive,
    );

    let workflow = coordinator.get_workflow(&request_id);
    assert_eq!(workflow.status, WorkflowStatus::Allocated);
    assert!(!workflow.delivery_confirmed);
    assert_eq!(workflow.payment_id, payment_id);
    assert_eq!(
        inventory.get_blood_unit(&unit_id).status,
        InvBloodStatus::Reserved
    );

    // Step 2 — confirm delivery.
    coordinator.confirm_delivery(
        &request_id,
        &admin,
        &String::from_str(&env, "Facility-A"),
    );

    let workflow = coordinator.get_workflow(&request_id);
    assert_eq!(workflow.status, WorkflowStatus::Delivered);
    assert!(workflow.delivery_confirmed);
    assert_eq!(
        inventory.get_blood_unit(&unit_id).status,
        InvBloodStatus::Delivered
    );

    // Step 3 — settle payment.
    coordinator.settle_payment(&request_id, &admin);

    let workflow = coordinator.get_workflow(&request_id);
    assert_eq!(workflow.status, WorkflowStatus::Settled);
    assert_eq!(payments.get_payment(&payment_id).status, PayStatus::Released);
}





