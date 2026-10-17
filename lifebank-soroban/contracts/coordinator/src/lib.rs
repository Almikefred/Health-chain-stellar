#![no_std]
#![deny(deprecated)]

/// Cross-contract coordinator for the HealthDonor workflow.
///
/// Canonical workflow sequence enforced here:
///   1. allocate_units  – Request must be Pending; reserves inventory units and
///                        binds the payment to the workflow
///   2. confirm_delivery – Workflow must be Allocated; marks units Delivered
///   3. settle_payment   – Workflow must be Delivered; releases escrowed payment
///
/// Any step that finds the prerequisite state missing returns an error and makes
/// no state changes, providing safe rollback semantics within a single transaction.
///
/// Two independent circuit breakers guard the workflow:
///   * `pause()` – blocks every state-mutating entry point.
///   * `emergency_halt()` – sets a separate flag that blocks the workflow
///     entry points until `clear_emergency_halt()` is called. `unpause()` never
///     clears the halt flag and `clear_emergency_halt()` never clears the pause
///     flag, so an incident can only be acknowledged explicitly.
mod error;
mod types;

#[cfg(test)]
mod test;

pub use error::CoordinatorError;
pub use types::{DataKey, ExcursionSummary, WorkflowRecord, WorkflowStatus};

use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, Address, Env, Map, String, Symbol, Vec,
};

/// Default workflow expiry window: 6 hours expressed in seconds.
/// After `allocate_units` is called, if `confirm_delivery` is never invoked
/// within this window, anyone may call `expire_workflow` to roll back the
/// allocation and free the reserved units and escrowed payment.
const WORKFLOW_TIMEOUT_SECS: u64 = 6 * 60 * 60;

const CONTRACT_VERSION: u32 = 1;

// ── Minimal interface types mirroring the domain contracts ────────────────────
// These allow the coordinator to inspect cross-contract return values without
// importing compiled WASMs. Soroban's generated decoder unpacks the returned
// `ScMap` with `map_unpack_to_linear_memory`, which requires the map to contain
// *exactly* the declared keys — so every field of the real structs must be
// mirrored here, in the same order. The domain contracts must keep these in sync.

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BloodType {
    APositive,
    ANegative,
    BPositive,
    BNegative,
    ABPositive,
    ABNegative,
    OPositive,
    ONegative,
}

impl BloodType {
    pub fn can_donate_to(&self, recipient: &BloodType) -> bool {
        use BloodType::*;
        matches!(
            (self, recipient),
            (ONegative, _)
                | (OPositive, APositive | BPositive | ABPositive | OPositive)
                | (ANegative, APositive | ANegative | ABPositive | ABNegative)
                | (APositive, APositive | ABPositive)
                | (BNegative, BPositive | BNegative | ABPositive | ABNegative)
                | (BPositive, BPositive | ABPositive)
                | (ABNegative, ABPositive | ABNegative)
                | (ABPositive, ABPositive)
        )
    }
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BloodComponent {
    WholeBlood,
    RedCells,
    Plasma,
    Platelets,
    Cryoprecipitate,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Urgency {
    Critical,
    Urgent,
    Routine,
    Scheduled,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestStatus {
    Pending,
    Approved,
    InProgress,
    Fulfilled,
    Cancelled,
    Rejected,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestHistoryEntry {
    pub previous_status: RequestStatus,
    pub is_initial_transition: bool,
    pub new_status: RequestStatus,
    pub actor: Address,
    pub reason: String,
    pub fulfilled_delta_ml: u32,
    pub released_reservation: bool,
    pub timestamp: u64,
}

/// Mirrors `requests::types::BloodRequest` (14 fields).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BloodRequest {
    pub id: u64,
    pub hospital_id: Address,
    pub blood_type: BloodType,
    pub component: BloodComponent,
    pub quantity_ml: u32,
    pub urgency: Urgency,
    pub created_timestamp: u64,
    pub required_by_timestamp: u64,
    pub status: RequestStatus,
    pub assigned_units: Vec<u64>,
    pub fulfilled_quantity_ml: u32,
    pub reservation_id: Option<u64>,
    pub fulfilled_by: Option<Address>,
    pub history: Vec<RequestHistoryEntry>,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BloodStatus {
    Available,
    Reserved,
    InTransit,
    Delivered,
    Expired,
    Compromised,
    Disposed,
}

/// Mirrors `inventory::types::BloodUnit` (9 fields).
#[contracttype]
#[derive(Clone, Debug)]
pub struct BloodUnit {
    pub id: u64,
    pub blood_type: BloodType,
    pub quantity_ml: u32,
    pub bank_id: Address,
    pub donor_id: Option<Address>,
    pub donation_timestamp: u64,
    pub expiration_timestamp: u64,
    pub status: BloodStatus,
    pub metadata: Map<Symbol, String>,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentStatus {
    Pending,
    Locked,
    Released,
    Refunded,
    Disputed,
    Cancelled,
}

/// Mirrors `payments::Payment` (12 fields).
#[contracttype]
#[derive(Clone, Debug)]
pub struct Payment {
    pub id: u64,
    pub request_id: u64,
    pub payer: Address,
    pub payee: Address,
    pub amount: i128,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub dispute_reason_code: Option<u32>,
    pub dispute_case_id: Option<String>,
    pub dispute_resolved: bool,
    pub token: Option<Address>,
}

// ── Cross-contract client traits ──────────────────────────────────────────────

mod request_client {
    use super::BloodRequest;
    use soroban_sdk::{contractclient, Env};

    #[contractclient(name = "RequestContractClient")]
    #[allow(dead_code)]
    pub trait RequestContractInterface {
        fn get_request(env: Env, request_id: u64) -> BloodRequest;
    }
}

mod inventory_client {
    use super::{BloodStatus, BloodUnit};
    use soroban_sdk::{contractclient, Address, Env, String};

    #[contractclient(name = "InventoryContractClient")]
    #[allow(dead_code)]
    pub trait InventoryContractInterface {
        fn get_blood_unit(env: Env, blood_unit_id: u64) -> BloodUnit;
        fn update_status(
            env: Env,
            unit_id: u64,
            new_status: BloodStatus,
            authorized_by: Address,
            reason: Option<String>,
        ) -> BloodUnit;
        fn mark_delivered(
            env: Env,
            unit_id: u64,
            authorized_by: Address,
            delivery_location: String,
        ) -> BloodUnit;
    }
}

mod payment_client {
    use super::{Payment, PaymentStatus};
    use soroban_sdk::{contractclient, contracttype, Address, Env, String};

    #[contracttype]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum DisputeReason {
        FailedDelivery,
        TemperatureExcursion,
        PaymentContested,
        WrongItem,
        DamagedGoods,
        LateDelivery,
        Other,
    }

    #[contractclient(name = "PaymentContractClient")]
    #[allow(dead_code)]
    pub trait PaymentContractInterface {
        fn get_payment(env: Env, payment_id: u64) -> Payment;
        fn update_status(
            env: Env,
            payment_id: u64,
            status: PaymentStatus,
            caller: Address,
        );
        fn record_dispute(
            env: Env,
            payment_id: u64,
            reason: DisputeReason,
            case_id: String,
            caller: Address,
        );
    }
}

use inventory_client::InventoryContractClient;
use payment_client::PaymentContractClient;
use request_client::RequestContractClient;

// ── Contract events ───────────────────────────────────────────────────────────

#[contractevent(topics = ["coord", "init"], data_format = "single-value")]
pub struct CoordInitialized {
    pub admin: Address,
}

#[contractevent(topics = ["coord", "emrghlt"], data_format = "single-value")]
pub struct CoordEmergencyHalt {
    pub admin: Address,
}

#[contractevent(topics = ["coord", "alloc"], data_format = "vec")]
pub struct CoordAllocated {
    pub request_id: u64,
    pub unit_ids: Vec<u64>,
    pub unit_count: u32,
}

#[contractevent(topics = ["coord", "dlvrd"], data_format = "vec")]
pub struct CoordDelivered {
    pub request_id: u64,
    pub location: String,
}

#[contractevent(topics = ["coord", "settld"], data_format = "vec")]
pub struct CoordSettled {
    pub request_id: u64,
    pub payment_id: u64,
}

#[contractevent(topics = ["coord", "rollbk"], data_format = "single-value")]
pub struct CoordRolledBack {
    pub request_id: u64,
}

#[contractevent(topics = ["coord", "expired"], data_format = "single-value")]
pub struct CoordExpired {
    pub request_id: u64,
}

#[contractevent(topics = ["coord", "tmp_brch"], data_format = "vec")]
pub struct CoordTemperatureBreach {
    pub payment_id: u64,
    pub unit_id: u64,
    pub timestamp: u64,
}

// ── Storage helpers ────────────────────────────────────────────────────────────

fn get_admin(env: &Env) -> Result<Address, CoordinatorError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(CoordinatorError::NotInitialized)
}

fn require_admin(env: &Env, caller: &Address) -> Result<(), CoordinatorError> {
    if *caller != get_admin(env)? {
        return Err(CoordinatorError::Unauthorized);
    }
    Ok(())
}

fn get_contract_address(env: &Env, key: &DataKey) -> Result<Address, CoordinatorError> {
    env.storage()
        .instance()
        .get(key)
        .ok_or(CoordinatorError::NotInitialized)
}

fn is_terminal(status: WorkflowStatus) -> bool {
    matches!(status, WorkflowStatus::Settled | WorkflowStatus::RolledBack)
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn is_emergency_halted(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::EmergencyHalt)
        .unwrap_or(false)
}

fn require_not_paused(env: &Env) -> Result<(), CoordinatorError> {
    if is_paused(env) {
        return Err(CoordinatorError::ContractPaused);
    }
    Ok(())
}

/// Emergency halt is tracked independently from `Paused`: `unpause()` must never
/// silently lift a halt, and clearing a halt must never silently unpause.
fn require_not_emergency_halted(env: &Env) -> Result<(), CoordinatorError> {
    if is_emergency_halted(env) {
        return Err(CoordinatorError::EmergencyHalted);
    }
    Ok(())
}

fn load_workflow(env: &Env, request_id: u64) -> Option<WorkflowRecord> {
    const WORKFLOW_TTL_LEDGERS: u32 = 535_680; // ~30 days at 5s/ledger
    let key = DataKey::Workflow(request_id);

    let workflow: Option<WorkflowRecord> = env.storage().persistent().get(&key);
    if let Some(wf) = &workflow {
        // Terminal workflows are archived with a fixed short TTL at write time
        // (see save_workflow) and must be allowed to lapse naturally — do not
        // keep extending TTL on every read or storage grows unbounded forever.
        if !is_terminal(wf.status) {
            env.storage()
                .persistent()
                .extend_ttl(&key, WORKFLOW_TTL_LEDGERS, WORKFLOW_TTL_LEDGERS);
        }
    }

    workflow
}

fn save_workflow(env: &Env, wf: &WorkflowRecord) {
    const WORKFLOW_TTL_LEDGERS: u32 = 535_680; // ~30 days at 5s/ledger
    const TERMINAL_TTL_LEDGERS: u32 = 17_280; // ~1 day at 5s/ledger
    let key = DataKey::Workflow(wf.request_id);

    env.storage().persistent().set(&key, wf);

    // Terminal workflows are never mutated again, so give them a short TTL and
    // let them expire to keep persistent storage bounded.
    let ttl = if is_terminal(wf.status) {
        TERMINAL_TTL_LEDGERS
    } else {
        WORKFLOW_TTL_LEDGERS
    };
    env.storage().persistent().extend_ttl(&key, ttl, ttl);
}

/// Read a payment, mapping a missing/trapped lookup to `PaymentNotFound`.
///
/// `try_get_payment` is generated by `#[contractclient]` for the non-`Result`
/// `get_payment` declaration, so it returns a nested result: the outer layer
/// reports invocation failures, the inner one the decoded value.
fn load_payment(
    client: &PaymentContractClient,
    payment_id: u64,
) -> Result<Payment, CoordinatorError> {
    match client.try_get_payment(&payment_id) {
        Ok(Ok(payment)) => Ok(payment),
        _ => Err(CoordinatorError::PaymentNotFound),
    }
}

/// Release the workflow's units back to `Available`.
fn release_units(env: &Env, workflow: &WorkflowRecord, admin: &Address) -> Result<(), CoordinatorError> {
    let inventory_contract = get_contract_address(env, &DataKey::InventoryContract)?;
    let inventory_client = InventoryContractClient::new(env, &inventory_contract);
    for unit_id in workflow.unit_ids.iter() {
        inventory_client.update_status(&unit_id, &BloodStatus::Available, admin, &None);
    }
    Ok(())
}

#[contract]
pub struct CoordinatorContract;

#[contractimpl]
impl CoordinatorContract {
    // ── Initialization & admin ────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        admin: Address,
        request_contract: Address,
        inventory_contract: Address,
        payment_contract: Address,
    ) -> Result<(), CoordinatorError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(CoordinatorError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::RequestContract, &request_contract);
        env.storage()
            .instance()
            .set(&DataKey::InventoryContract, &inventory_contract);
        env.storage()
            .instance()
            .set(&DataKey::PaymentContract, &payment_contract);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage()
            .instance()
            .set(&DataKey::EmergencyHalt, &false);
        env.storage().instance().set(&DataKey::Version, &CONTRACT_VERSION);
        CoordInitialized { admin }.publish(&env);
        Ok(())
    }

    pub fn propose_admin(
        env: Env,
        admin: Address,
        new_admin: Address,
    ) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        Ok(())
    }

    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), CoordinatorError> {
        new_admin.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(CoordinatorError::Unauthorized)?;
        if new_admin != pending {
            return Err(CoordinatorError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        Ok(())
    }

    pub fn pause(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    /// Clears `DataKey::Paused` only. An active emergency halt stays in force
    /// until `clear_emergency_halt()` is called by the admin.
    pub fn unpause(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Raises the emergency halt. The halt flag is separate from the pause flag
    /// so it can be acknowledged independently: `unpause()` never clears it and
    /// `clear_emergency_halt()` never clears the pause flag.
    pub fn emergency_halt(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::EmergencyHalt, &true);
        CoordEmergencyHalt { admin }.publish(&env);
        Ok(())
    }

    /// Clears `DataKey::EmergencyHalt` only; `DataKey::Paused` is left for
    /// `unpause()` so both circuit breakers must be lifted deliberately.
    pub fn clear_emergency_halt(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::EmergencyHalt, &false);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }

    pub fn is_emergency_halted(env: Env) -> bool {
        is_emergency_halted(&env)
    }

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    pub fn get_admin_address(env: Env) -> Result<Address, CoordinatorError> {
        get_admin(&env)
    }

    pub fn version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Version)
            .unwrap_or(CONTRACT_VERSION)
    }

    // ── Configuration ─────────────────────────────────────────────────────────

    pub fn set_contract_addresses(
        env: Env,
        request_contract: Address,
        inventory_contract: Address,
        payment_contract: Address,
    ) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::RequestContract, &request_contract);
        env.storage()
            .instance()
            .set(&DataKey::InventoryContract, &inventory_contract);
        env.storage()
            .instance()
            .set(&DataKey::PaymentContract, &payment_contract);
        Ok(())
    }

    /// Configure the temperature-oracle contract allowed to raise disputes
    /// through `flag_temperature_breach` without being the coordinator admin.
    pub fn set_temperature_oracle(
        env: Env,
        admin: Address,
        oracle: Address,
    ) -> Result<(), CoordinatorError> {
        admin.require_auth();
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::TemperatureOracle, &oracle);
        Ok(())
    }

    // ── Workflow: allocate ────────────────────────────────────────────────────

    pub fn allocate_units(
        env: Env,
        request_id: u64,
        unit_ids: Vec<u64>,
        payment_id: u64,
        caller: Address,
        blood_type: BloodType,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        require_admin(&env, &caller)?;
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        if unit_ids.is_empty() {
            return Err(CoordinatorError::NoUnitsSpecified);
        }

        // Re-allocation is only allowed once the previous workflow is terminal.
        if let Some(existing) = load_workflow(&env, request_id) {
            if !is_terminal(existing.status) {
                return Err(CoordinatorError::WorkflowAlreadyStarted);
            }
        }

        let request_contract = get_contract_address(&env, &DataKey::RequestContract)?;
        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;

        let request_client = RequestContractClient::new(&env, &request_contract);
        let request = request_client.get_request(&request_id);
        if request.status != RequestStatus::Pending {
            return Err(CoordinatorError::InvalidRequestState);
        }
        if blood_type != request.blood_type {
            return Err(CoordinatorError::IncompatibleBloodType);
        }

        // The payment must belong to this request; otherwise an operator error
        // could bind, and later release, an unrelated hospital's escrow.
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        let payment = load_payment(&payment_client, payment_id)?;
        if payment.request_id != request_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }

        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);
        let mut unit_count: u32 = 0;
        for unit_id in unit_ids.iter() {
            let unit = inventory_client.get_blood_unit(&unit_id);
            if unit.status != BloodStatus::Available {
                return Err(CoordinatorError::UnitNotAvailable);
            }
            if !unit.blood_type.can_donate_to(&blood_type) {
                return Err(CoordinatorError::IncompatibleBloodType);
            }
            inventory_client.update_status(
                &unit_id,
                &BloodStatus::Reserved,
                &caller,
                &None,
            );
            unit_count += 1;
        }

        let now = env.ledger().timestamp();
        let workflow = WorkflowRecord {
            request_id,
            payment_id,
            unit_ids: unit_ids.clone(),
            status: WorkflowStatus::Allocated,
            delivery_confirmed: false,
            delivery_location: None,
            expires_at: now + WORKFLOW_TIMEOUT_SECS,
        };
        save_workflow(&env, &workflow);

        CoordAllocated {
            request_id,
            unit_ids,
            unit_count,
        }
        .publish(&env);

        Ok(())
    }

    // ── Workflow: confirm delivery ────────────────────────────────────────────

    pub fn confirm_delivery(
        env: Env,
        request_id: u64,
        caller: Address,
        location: String,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        require_admin(&env, &caller)?;
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowState);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);

        for unit_id in workflow.unit_ids.iter() {
            // The inventory state machine requires Reserved → InTransit → Delivered.
            inventory_client.update_status(
                &unit_id,
                &BloodStatus::InTransit,
                &caller,
                &None,
            );
            inventory_client.mark_delivered(&unit_id, &caller, &location);
        }

        workflow.status = WorkflowStatus::Delivered;
        workflow.delivery_confirmed = true;
        workflow.delivery_location = Some(location.clone());
        save_workflow(&env, &workflow);

        CoordDelivered {
            request_id,
            location,
        }
        .publish(&env);

        Ok(())
    }

    // ── Workflow: settle payment ──────────────────────────────────────────────

    pub fn settle_payment(
        env: Env,
        request_id: u64,
        caller: Address,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        require_admin(&env, &caller)?;
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Delivered || !workflow.delivery_confirmed {
            return Err(CoordinatorError::DeliveryNotConfirmed);
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);

        // Only the payment bound to this workflow, for this request, may be
        // released.
        let payment = load_payment(&payment_client, workflow.payment_id)?;
        if payment.request_id != request_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }
        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        payment_client.update_status(
            &workflow.payment_id,
            &PaymentStatus::Released,
            &caller,
        );

        workflow.status = WorkflowStatus::Settled;
        save_workflow(&env, &workflow);

        CoordSettled {
            request_id,
            payment_id: workflow.payment_id,
        }
        .publish(&env);

        Ok(())
    }

    // ── Workflow: rollback ────────────────────────────────────────────────────

    pub fn rollback(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        match workflow.status {
            WorkflowStatus::Settled => return Err(CoordinatorError::CannotRollbackSettled),
            // Units already handed over to a hospital must never be released
            // back into inventory.
            WorkflowStatus::Allocated => {}
            _ => return Err(CoordinatorError::InvalidWorkflowState),
        }

        release_units(&env, &workflow, &admin)?;

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        if let Ok(payment) = load_payment(&payment_client, workflow.payment_id) {
            if payment.status == PaymentStatus::Locked {
                payment_client.update_status(
                    &workflow.payment_id,
                    &PaymentStatus::Refunded,
                    &admin,
                );
            }
        }

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordRolledBack { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: expiry ──────────────────────────────────────────────────────

    pub fn expire_workflow(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowState);
        }

        if env.ledger().timestamp() < workflow.expires_at {
            return Err(CoordinatorError::WorkflowNotExpired);
        }

        let admin = get_admin(&env)?;
        release_units(&env, &workflow, &admin)?;

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        if let Ok(payment) = load_payment(&payment_client, workflow.payment_id) {
            if payment.status == PaymentStatus::Locked {
                payment_client.update_status(
                    &workflow.payment_id,
                    &PaymentStatus::Refunded,
                    &admin,
                );
            }
        }

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordExpired { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: temperature breach ──────────────────────────────────────────

    /// Raise a temperature-excursion dispute against the payment bound to a
    /// workflow. Only the coordinator admin may call this, and only for a unit
    /// that belongs to the workflow and the payment bound to the request — the
    /// previous implementation disputed any payment id supplied by the caller.
    pub fn report_temperature_breach(
        env: Env,
        request_id: u64,
        unit_id: u64,
        payment_id: u64,
    ) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if is_terminal(workflow.status) {
            return Err(CoordinatorError::InvalidWorkflowState);
        }

        if !workflow.unit_ids.contains(unit_id) {
            return Err(CoordinatorError::UnitNotFound);
        }

        // The payment must be the one bound at allocation, and it must belong to
        // this request. Together these prevent disputing an unrelated hospital's
        // escrow.
        if payment_id != workflow.payment_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        let payment = load_payment(&payment_client, payment_id)?;
        if payment.request_id != request_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }
        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        payment_client.record_dispute(
            &payment_id,
            &payment_client::DisputeReason::TemperatureExcursion,
            &String::from_str(&env, "temperature-breach"),
            &admin,
        );

        CoordTemperatureBreach {
            payment_id,
            unit_id,
            timestamp: env.ledger().timestamp(),
        }
        .publish(&env);

        Ok(())
    }

    /// Dispute a locked payment on behalf of the temperature oracle (or the
    /// admin). Used by the temperature contract when a sustained excursion is
    /// confirmed; it has no workflow context, so only the payment id is bound.
    pub fn flag_temperature_breach(
        env: Env,
        caller: Address,
        payment_id: u64,
        excursion_summary: ExcursionSummary,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();

        let admin = get_admin(&env)?;
        let oracle: Option<Address> = env.storage().instance().get(&DataKey::TemperatureOracle);
        let authorized = caller == admin || oracle.map(|o| o == caller).unwrap_or(false);
        if !authorized {
            return Err(CoordinatorError::Unauthorized);
        }

        require_not_paused(&env)?;
        require_not_emergency_halted(&env)?;

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        let payment = load_payment(&payment_client, payment_id)?;
        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        payment_client.record_dispute(
            &payment_id,
            &payment_client::DisputeReason::TemperatureExcursion,
            &String::from_str(&env, "temperature-breach"),
            &caller,
        );

        CoordTemperatureBreach {
            payment_id,
            unit_id: excursion_summary.unit_id,
            timestamp: env.ledger().timestamp(),
        }
        .publish(&env);

        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn get_workflow(env: Env, request_id: u64) -> Result<WorkflowRecord, CoordinatorError> {
        load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)
    }
}
