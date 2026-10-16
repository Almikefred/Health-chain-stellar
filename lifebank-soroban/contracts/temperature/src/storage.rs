use crate::types::{DataKey, TemperatureReading, TemperatureThreshold};
use soroban_sdk::{Address, Env, Vec};

/// TTL constants for persistent temperature entries (in ledgers; ~5 s each).
/// Entries are bumped whenever their remaining TTL falls below the threshold.
const TEMP_BUMP_THRESHOLD: u32 = 518_400; // ~30 days
const TEMP_BUMP_TO: u32 = 1_036_800; // ~60 days

pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
}

pub fn get_admin(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .expect("admin not set")
}

pub fn set_threshold(env: &Env, unit_id: u64, threshold: &TemperatureThreshold) {
    let key = DataKey::Threshold(unit_id);
    env.storage().persistent().set(&key, threshold);
    env.storage()
        .persistent()
        .extend_ttl(&key, TEMP_BUMP_THRESHOLD, TEMP_BUMP_TO);
}

pub fn get_threshold(env: &Env, unit_id: u64) -> Option<TemperatureThreshold> {
    env.storage().persistent().get(&DataKey::Threshold(unit_id))
}

pub fn set_temp_page(env: &Env, unit_id: u64, page: u32, readings: &Vec<TemperatureReading>) {
    let key = DataKey::TempPage(unit_id, page);
    env.storage().persistent().set(&key, readings);
    env.storage()
        .persistent()
        .extend_ttl(&key, TEMP_BUMP_THRESHOLD, TEMP_BUMP_TO);
}

/// Returns the readings stored on `page`, or an empty Vec if the page has
/// never been written.
pub fn get_temp_page(env: &Env, unit_id: u64, page: u32) -> Vec<TemperatureReading> {
    env.storage()
        .persistent()
        .get(&DataKey::TempPage(unit_id, page))
        .unwrap_or_else(|| Vec::new(env))
}

pub fn set_temp_page_len(env: &Env, unit_id: u64, page: u32, len: u32) {
    let key = DataKey::TempPageLen(unit_id, page);
    env.storage().persistent().set(&key, &len);
    env.storage()
        .persistent()
        .extend_ttl(&key, TEMP_BUMP_THRESHOLD, TEMP_BUMP_TO);
}

pub fn get_temp_page_len(env: &Env, unit_id: u64, page: u32) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::TempPageLen(unit_id, page))
        .unwrap_or(0)
}

pub fn get_current_page(env: &Env, unit_id: u64) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::CurrentPage(unit_id))
        .unwrap_or(0)
}

pub fn set_current_page(env: &Env, unit_id: u64, page: u32) {
    let key = DataKey::CurrentPage(unit_id);
    env.storage().persistent().set(&key, &page);
    env.storage()
        .persistent()
        .extend_ttl(&key, TEMP_BUMP_THRESHOLD, TEMP_BUMP_TO);
}
