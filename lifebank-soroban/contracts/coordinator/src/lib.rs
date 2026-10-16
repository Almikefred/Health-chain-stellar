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
pub use payment_client::DisputeReason;
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
        /// Escrow-aware refund: moves the locked amount back to the payer.
        /// `update_status` cannot be used for escrow payments because it
        /// performs no token transfer and is rejected with
        /// `EscrowSettlementRequired` by the payments contract.
        fn refund_escrow(env: Env, caller: Address, payment_id: u64);
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

/// Require `caller` to be the stored admin, rejecting any other address even
/// when its own signature is present.
fn require_admin(env: &Env, caller: &Address) -> Result<(), CoordinatorError> {
    caller.require_auth();
    let admin = get_admin(env)?;
    if caller != &admin {
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

/// Refund the escrow bound to `workflow` when it is still `Locked`.
///
/// The payment record must belong to the same request — a workflow can never
/// refund a stranger's escrow. Payments in any other state are left untouched
/// (they carry no locked funds this workflow is responsible for).
fn refund_locked_payment(
    env: &Env,
    workflow: &WorkflowRecord,
    admin: &Address,
) -> Result<(), CoordinatorError> {
    let payment_contract = get_contract_address(env, &DataKey::PaymentContract)?;
    let payment_client = PaymentContractClient::new(env, &payment_contract);

    let payment = payment_client
        .try_get_payment(&workflow.payment_id)
        .map_err(|_| CoordinatorError::PaymentNotFound)?
        .map_err(|_| CoordinatorError::PaymentNotFound)?;

    if payment.request_id != workflow.request_id {
        return Err(CoordinatorError::PaymentRequestMismatch);
    }

    if payment.status == PaymentStatus::Locked {
        // Escrow-aware path: performs the token transfer back to the payer.
        payment_client.refund_escrow(admin, &workflow.payment_id);
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
        env.storage().instance().set(&DataKey::EmergencyHalt, &false);

        CoordInitialized { admin }.publish(&env);
        Ok(())
    }

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    pub fn propose_admin(
        env: Env,
        admin: Address,
        new_admin: Address,
    ) -> Result<(), CoordinatorError> {
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
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Emergency halt blocks `confirm_delivery` and `settle_payment` for every
    /// in-flight workflow until the admin clears it.
    pub fn emergency_halt(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::EmergencyHalt, &true);
        CoordEmergencyHalt { admin }.publish(&env);
        Ok(())
    }

    pub fn clear_emergency_halt(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::EmergencyHalt, &false);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn is_emergency_halted(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::EmergencyHalt)
            .unwrap_or(false)
    }

    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    // ── Configuration ─────────────────────────────────────────────────────────

    /// Configure the temperature-oracle address allowed to escalate a sustained
    /// cold-chain excursion into a payment dispute.
    pub fn set_temperature_oracle(
        env: Env,
        admin: Address,
        oracle: Address,
    ) -> Result<(), CoordinatorError> {
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
        require_admin(&env, &caller)?;

        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        if unit_ids.is_empty() {
            return Err(CoordinatorError::NoUnitsSpecified);
        }

        // A terminal workflow (Settled / RolledBack) may be replaced by a new
        // allocation for the same request; an in-flight one may not.
        if let Some(existing) = load_workflow(&env, request_id) {
            if !is_terminal(existing.status) {
                return Err(CoordinatorError::WorkflowAlreadyStarted);
            }
        }

        let request_contract = get_contract_address(&env, &DataKey::RequestContract)?;
        let request_client = RequestContractClient::new(&env, &request_contract);
        let request = request_client
            .try_get_request(&request_id)
            .map_err(|_| CoordinatorError::RequestNotFound)?
            .map_err(|_| CoordinatorError::RequestNotFound)?;
        if request.status != RequestStatus::Pending {
            return Err(CoordinatorError::InvalidRequestState);
        }

        // Bind and validate the escrow up front so rollback/expiry know exactly
        // which payment to refund. A non-Locked payment is allowed here (escrow
        // may not be funded yet); it simply has nothing to refund later.
        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);
        let payment = payment_client
            .try_get_payment(&payment_id)
            .map_err(|_| CoordinatorError::PaymentNotFound)?
            .map_err(|_| CoordinatorError::PaymentNotFound)?;
        if payment.request_id != request_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);

        let mut unit_count: u32 = 0;
        for unit_id in unit_ids.iter() {
            let unit = inventory_client
                .try_get_blood_unit(&unit_id)
                .map_err(|_| CoordinatorError::UnitNotFound)?
                .map_err(|_| CoordinatorError::UnitNotFound)?;
            if unit.status != BloodStatus::Available {
                return Err(CoordinatorError::UnitNotAvailable);
            }
            if !unit.blood_type.can_donate_to(&blood_type) {
                return Err(CoordinatorError::IncompatibleBloodType);
            }
            unit_count = unit_count.saturating_add(1);
        }

        for unit_id in unit_ids.iter() {
            inventory_client.update_status(
                &unit_id,
                &BloodStatus::Reserved,
                &caller,
                &None,
            );
        }

        let workflow = WorkflowRecord {
            request_id,
            payment_id,
            unit_ids: unit_ids.clone(),
            status: WorkflowStatus::Allocated,
            delivery_confirmed: false,
            delivery_location: None,
            expires_at: env
                .ledger()
                .timestamp()
                .saturating_add(WORKFLOW_TIMEOUT_SECS),
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
        require_admin(&env, &caller)?;

        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }
        if Self::is_emergency_halted(env.clone()) {
            return Err(CoordinatorError::EmergencyHalted);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;
        if workflow.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowStatus);
        }

        let inventory_contract = get_contract_address(&env, &DataKey::InventoryContract)?;
        let inventory_client = InventoryContractClient::new(&env, &inventory_contract);

        for unit_id in workflow.unit_ids.iter() {
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
        require_admin(&env, &caller)?;

        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }
        if Self::is_emergency_halted(env.clone()) {
            return Err(CoordinatorError::EmergencyHalted);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        match workflow.status {
            WorkflowStatus::Delivered => {}
            WorkflowStatus::Allocated => return Err(CoordinatorError::DeliveryNotConfirmed),
            _ => return Err(CoordinatorError::InvalidWorkflowStatus),
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);

        let payment = payment_client
            .try_get_payment(&workflow.payment_id)
            .map_err(|_| CoordinatorError::PaymentNotFound)?
            .map_err(|_| CoordinatorError::PaymentNotFound)?;

        if payment.request_id != workflow.request_id {
            return Err(CoordinatorError::PaymentRequestMismatch);
        }
        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        payment_client.update_status(&workflow.payment_id, &PaymentStatus::Released, &caller);

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

    /// Admin-only rollback of an in-flight allocation.
    ///
    /// Only an `Allocated` workflow may be rolled back: once delivery has been
    /// confirmed the units are physically at the hospital, so releasing them
    /// back to `Available` (and refunding the payment) would let the same unit
    /// be re-allocated elsewhere while a copy is already delivered.
    pub fn rollback(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        let admin = get_admin(&env)?;
        admin.require_auth();

        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        match workflow.status {
            WorkflowStatus::Allocated => {}
            WorkflowStatus::Settled | WorkflowStatus::RolledBack => {
                return Err(CoordinatorError::CannotRollbackSettled)
            }
            WorkflowStatus::Delivered | WorkflowStatus::Pending => {
                return Err(CoordinatorError::InvalidWorkflowState)
            }
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

        // Return the escrowed payment to the payer before marking the workflow
        // terminal, so no locked funds are orphaned.
        refund_locked_payment(&env, &workflow, &admin)?;

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordRolledBack { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: expiry ──────────────────────────────────────────────────────

    /// Anyone may roll back an `Allocated` workflow whose 6-hour deadline has
    /// elapsed. Blocked while the pause circuit breaker is active.
    pub fn expire_workflow(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        let mut workflow =
            load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        match workflow.status {
            WorkflowStatus::Allocated => {}
            WorkflowStatus::Delivered => return Err(CoordinatorError::InvalidWorkflowState),
            _ => return Err(CoordinatorError::InvalidWorkflowStatus),
        }

        if env.ledger().timestamp() < workflow.expires_at {
            return Err(CoordinatorError::WorkflowNotExpired);
        }

        let admin = get_admin(&env)?;
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

        refund_locked_payment(&env, &workflow, &admin)?;

        workflow.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &workflow);

        CoordExpired { request_id }.publish(&env);

        Ok(())
    }

    // ── Workflow: temperature breach ──────────────────────────────────────────

    /// Called by the temperature contract when a sustained excursion is
    /// verified. Only the configured oracle contract (or the admin) may flag a
    /// breach; the caller-supplied identity is never trusted blindly.
    pub fn flag_temperature_breach(
        env: Env,
        caller: Address,
        payment_id: u64,
        excursion_summary: ExcursionSummary,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();

        if Self::is_paused(env.clone()) {
            return Err(CoordinatorError::ContractPaused);
        }

        let admin = get_admin(&env)?;
        let oracle: Option<Address> = env.storage().instance().get(&DataKey::TemperatureOracle);
        if caller != admin && oracle != Some(caller.clone()) {
            return Err(CoordinatorError::Unauthorized);
        }

        let payment_contract = get_contract_address(&env, &DataKey::PaymentContract)?;
        let payment_client = PaymentContractClient::new(&env, &payment_contract);

        let payment = payment_client
            .try_get_payment(&payment_id)
            .map_err(|_| CoordinatorError::PaymentNotFound)?
            .map_err(|_| CoordinatorError::PaymentNotFound)?;

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
