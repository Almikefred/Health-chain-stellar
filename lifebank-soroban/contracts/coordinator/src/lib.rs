#![no_std]
#![deny(deprecated)]

/// Cross-contract coordinator for the HealthDonor workflow.
///
/// Canonical workflow sequence enforced here:
///   1. allocate_units  – Request must be Pending; reserves inventory units
///   2. confirm_delivery – Workflow must be Allocated; marks units Delivered
///   3. settle_payment   – Workflow must be Delivered; releases escrowed payment
///
/// Any step that finds the prerequisite state missing returns an error and makes
/// no state changes, providing safe rollback semantics within a single transaction.
mod error;
mod types;

#[cfg(test)]
mod test;

pub use error::CoordinatorError;
pub use types::{DataKey, ExcursionSummary, WorkflowRecord, WorkflowStatus};

use soroban_sdk::{contract, contractevent, contractimpl, contracttype, Address, Env, String, Vec};

/// Default workflow expiry window: 6 hours expressed in seconds.
/// After `allocate_units` is called, if `confirm_delivery` is never invoked
/// within this window, anyone may call `expire_workflow` to roll back the
/// allocation and free the reserved units and escrowed payment.
const WORKFLOW_TIMEOUT_SECS: u64 = 6 * 60 * 60;

const CONTRACT_VERSION: u32 = 1;

// ── Minimal interface types mirroring the domain contracts ────────────────────
// These allow the coordinator to inspect cross-contract return values without
// importing compiled WASMs. The domain contracts must keep these in sync.

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
pub enum RequestStatus {
    Pending,
    Approved,
    Fulfilled,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BloodRequest {
    pub id: u64,
    pub status: RequestStatus,
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

#[contracttype]
#[derive(Clone, Debug)]
pub struct BloodUnit {
    pub id: u64,
    pub status: BloodStatus,
    pub blood_type: BloodType,
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

#[contracttype]
#[derive(Clone, Debug)]
pub struct Payment {
    pub id: u64,
    pub request_id: u64,
    pub status: PaymentStatus,
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
        fn get_admin(env: Env) -> Address;
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

fn get_contract_address(env: &Env, key: &DataKey) -> Result<Address, CoordinatorError> {
    env.storage()
        .instance()
        .get(key)
        .ok_or(CoordinatorError::NotInitialized)
}

fn is_terminal(status: WorkflowStatus) -> bool {
    matches!(status, WorkflowStatus::Settled | WorkflowStatus::RolledBack)
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

#[contract]
pub struct CoordinatorContract;

#[contractimpl]
impl CoordinatorContract {
    // ── Initialization & admin ────────────────────────────────────────────────

    pub fn initialize(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(CoordinatorError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Version, &CONTRACT_VERSION);
        CoordInitialized { admin }.publish(&env);
        Ok(())
    }

    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
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

    pub fn pause(env: Env) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    pub fn emergency_halt(env: Env) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        CoordEmergencyHalt { admin }.publish(&env);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
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

    // ── Workflow: allocate ────────────────────────────────────────────────────

    pub fn allocate_units(
        env: Env,
        request_id: u64,
        unit_ids: Vec<u64>,
    ) -> Result<(), CoordinatorError> {
        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        if load_workflow(&env, request_id).is_some() {
            return Err(CoordinatorError::WorkflowAlreadyExists);
        }

        let request_contract = get_contract_address(&env, &DataKey::RequestContract)?;
        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;

        let request_client = RequestContractClient::new(&env, &request_contract);
        let request = request_client.get_request(&request_id);
        if request.status != RequestStatus::Pending {
            return Err(CoordinatorError::InvalidRequestStatus);
        }

        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);
        let mut unit_count: u32 = 0;
        for unit_id in unit_ids.iter() {
            let unit = inventory_client.get_blood_unit(&unit_id);
            if unit.status != BloodStatus::Available {
                return Err(CoordinatorError::UnitNotAvailable);
            }
            unit_count += 1;
        }

        let workflow = WorkflowRecord {
            request_id,
            status: WorkflowStatus::Allocated,
            unit_ids: unit_ids.clone(),
            payment_id: 0,
            allocated_at: env.ledger().timestamp(),
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
        location: String,
    ) -> Result<(), CoordinatorError> {
        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);
        let admin = get_admin(&env)?;

        for unit_id in workflow.unit_ids.iter() {
            inventory_client.mark_delivered(&unit_id, &admin, &location);
        }

        workflow.status = WorkflowStatus::Delivered;
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
        payment_id: u64,
    ) -> Result<(), CoordinatorError> {
        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Delivered {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        let admin = get_admin(&env)?;

        payment_client.update_status(&payment_id, &PaymentStatus::Released, &admin);

        workflow.status = WorkflowStatus::Settled;
        workflow.payment_id = payment_id;
        save_workflow(&env, &workflow);

        CoordSettled {
            request_id,
            payment_id,
        }
        .publish(&env);

        Ok(())
    }

    // ── Workflow: rollback ────────────────────────────────────────────────────

    pub fn rollback_workflow(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if is_terminal(workflow.status) {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);

        for unit_id in workflow.unit_ids.iter() {
            inventory_client.update_status(
                &unit_id,
                &BloodStatus::Available,
                &admin,
                &None,
            );
        }

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordRolledBack { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: expiry ──────────────────────────────────────────────────────

    pub fn expire_workflow(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let now = env.ledger().timestamp();
        if now < workflow.allocated_at + WORKFLOW_TIMEOUT_SECS {
            return Err(CoordinatorError::WorkflowNotExpired);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);
        let admin = get_admin(&env)?;

        for unit_id in workflow.unit_ids.iter() {
            inventory_client.update_status(
                &unit_id,
                &BloodStatus::Available,
                &admin,
                &None,
            );
        }

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordExpired { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: temperature breach ──────────────────────────────────────────

    pub fn report_temperature_breach(
        env: Env,
        request_id: u64,
        unit_id: u64,
        payment_id: u64,
    ) -> Result<(), CoordinatorError> {
        let caller = env.current_contract_address();
        caller.require_auth();

        let workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if is_terminal(workflow.status) {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);

        payment_client.record_dispute(
            &payment_id,
            &payment_client::DisputeReason::TemperatureExcursion,
            &String::from_str(&env, "temperature-breach"),
            &caller,
        );

        CoordTemperatureBreach {
            payment_id,
            unit_id,
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
