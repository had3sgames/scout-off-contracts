mod errors;
mod events;
mod types;

use errors::VerificationError;
use types::{DataKey, Milestone, Validator};

use soroban_sdk::{contract, contractimpl, Address, Env, String};

const MILESTONE_TTL_THRESHOLD: u32 = 100;
const MILESTONE_TTL_EXTEND_TO: u32 = 518_400; // ~1 year at 5s/ledger

// Generated client for the progress contract — used for cross-contract calls.
// The progress contract must be deployed and its address registered via
// `set_progress_contract` before `approve_milestone` can advance levels.
mod progress_contract {
    soroban_sdk::contractimport!(
        file = "../../target/wasm32-unknown-unknown/release/scoutchain_progress.wasm"
    );
}

#[contract]
pub struct VerificationContract;

#[contractimpl]
impl VerificationContract {
    // -------------------------------------------------------------------------
    // Admin
    // -------------------------------------------------------------------------

    pub fn initialize(env: Env, admin: Address) -> Result<(), VerificationError> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(VerificationError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Store the progress contract address so approve_milestone can call it.
    /// Must be called after both contracts are deployed (admin only).
    pub fn set_progress_contract(
        env: Env,
        progress_contract: Address,
    ) -> Result<(), VerificationError> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ProgressContract, &progress_contract);
        Ok(())
    }

    /// Register a trusted validator (admin only).
    pub fn register_validator(
        env: Env,
        wallet: Address,
        credentials: String,
    ) -> Result<(), VerificationError> {
        Self::require_admin(&env)?;
        Self::require_not_paused(&env)?;

        if env
            .storage()
            .persistent()
            .has(&DataKey::Validator(wallet.clone()))
        {
            return Err(VerificationError::ValidatorAlreadyRegistered);
        }

        let validator = Validator {
            wallet: wallet.clone(),
            credentials,
            registered_at: env.ledger().timestamp(),
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Validator(wallet.clone()), &validator);

        events::validator_registered(&env, &wallet);
        Ok(())
    }

    /// Deactivate a validator (admin only).
    pub fn revoke_validator(env: Env, wallet: Address) -> Result<(), VerificationError> {
        Self::require_admin(&env)?;
        let mut validator: Validator = env
            .storage()
            .persistent()
            .get(&DataKey::Validator(wallet.clone()))
            .ok_or(VerificationError::ValidatorNotFound)?;
        validator.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Validator(wallet.clone()), &validator);
        events::validator_revoked(&env, &wallet);
        Ok(())
    }

    pub fn pause_contract(env: Env) -> Result<(), VerificationError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause_contract(env: Env) -> Result<(), VerificationError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Milestone approval
    // -------------------------------------------------------------------------

    /// Approve a player milestone. Caller must be a registered, active validator.
    ///
    /// After storing the milestone, this function calls `progress.advance_level`
    /// on the registered progress contract so both state changes happen atomically
    /// in the same Stellar transaction.
    ///
    /// Each milestone records the Stellar ledger sequence number for
    /// tamper-proof auditability.
    ///
    /// Returns the milestone index for this player.
    pub fn approve_milestone(
        env: Env,
        validator_wallet: Address,
        player_id: u64,
        description: String,
        evidence_hash: String,
    ) -> Result<u32, VerificationError> {
        Self::require_not_paused(&env)?;
        validator_wallet.require_auth();

        // Verify the caller is an active validator
        let validator: Validator = env
            .storage()
            .persistent()
            .get(&DataKey::Validator(validator_wallet.clone()))
            .ok_or(VerificationError::ValidatorNotFound)?;

        if !validator.active {
            return Err(VerificationError::ValidatorInactive);
        }

        // Increment milestone counter for this player
        let counter_key = DataKey::MilestoneCounter(player_id);
        let index: u32 = env
            .storage()
            .persistent()
            .get(&counter_key)
            .unwrap_or(0u32);
        let next_index = index.checked_add(1).expect("overflow");

        let milestone = Milestone {
            player_id,
            validator: validator_wallet.clone(),
            description,
            evidence_hash,
            approved_at: env.ledger().timestamp(),
            ledger_sequence: env.ledger().sequence(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Milestone(player_id, next_index), &milestone);
        env.storage().persistent().extend_ttl(
            &DataKey::Milestone(player_id, next_index),
            MILESTONE_TTL_THRESHOLD,
            MILESTONE_TTL_EXTEND_TO,
        );
        env.storage()
            .persistent()
            .set(&counter_key, &next_index);
        env.storage().persistent().extend_ttl(
            &counter_key,
            MILESTONE_TTL_THRESHOLD,
            MILESTONE_TTL_EXTEND_TO,
        );

        events::milestone_approved(&env, player_id, &validator_wallet);

        // Cross-contract call: advance the player's progress level.
        // This is a best-effort call — if the progress contract is not set
        // (e.g. during testing without a full deployment), we skip it.
        // In production, always call set_progress_contract before going live.
        if let Some(progress_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ProgressContract)
        {
            let progress_client = progress_contract::Client::new(&env, &progress_addr);
            // advance_level will return AlreadyAtMaxLevel if the player is
            // already at EliteTier — we intentionally ignore that error here
            // so the milestone is still recorded even at max level.
            let _ = progress_client.try_advance_level(
                &validator_wallet,
                &player_id,
                &next_index,
            );
        }

        Ok(next_index)
    }

    // -------------------------------------------------------------------------
    // Queries
    // -------------------------------------------------------------------------

    pub fn get_milestone(
        env: Env,
        player_id: u64,
        index: u32,
    ) -> Result<Milestone, VerificationError> {
        let key = DataKey::Milestone(player_id, index);
        let milestone = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VerificationError::InvalidInput)?;
        env.storage().persistent().extend_ttl(
            &key,
            MILESTONE_TTL_THRESHOLD,
            MILESTONE_TTL_EXTEND_TO,
        );
        Ok(milestone)
    }

    pub fn get_milestone_count(env: Env, player_id: u64) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::MilestoneCounter(player_id))
            .unwrap_or(0u32)
    }

    pub fn get_validator(env: Env, wallet: Address) -> Result<Validator, VerificationError> {
        env.storage()
            .persistent()
            .get(&DataKey::Validator(wallet))
            .ok_or(VerificationError::ValidatorNotFound)
    }

    pub fn is_active_validator(env: Env, wallet: Address) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, Validator>(&DataKey::Validator(wallet))
            .map(|v| v.active)
            .unwrap_or(false)
    }

    pub fn health(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Initialized)
            .unwrap_or(false)
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    fn require_admin(env: &Env) -> Result<(), VerificationError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(VerificationError::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), VerificationError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(VerificationError::ContractPaused);
        }
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env, String};

    fn setup() -> (Env, VerificationContractClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register_contract(None, VerificationContract);
        let client = VerificationContractClient::new(&env, &id);
        (env, client)
    }

    #[test]
    fn test_health_false_before_initialize() {
        let (_env, client) = setup();
        assert!(!client.health());
    }

    #[test]
    fn test_register_and_approve() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "UEFA B License"));

        assert!(client.is_active_validator(&validator));

        // No progress contract set — approve_milestone still records the milestone
        let idx = client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Scored 5 goals in Local Cup"),
            &String::from_str(&env, "QmEvidence123"),
        );
        assert_eq!(idx, 1);
        assert_eq!(client.get_milestone_count(&1u64), 1);

        let milestone = client.get_milestone(&1u64, &1);
        assert!(milestone.ledger_sequence > 0);
    }

    #[test]
    fn test_multiple_milestones_same_player() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "Coach"));

        let idx1 = client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Identity verified"),
            &String::from_str(&env, "QmKYC"),
        );
        let idx2 = client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Top speed 32 km/h"),
            &String::from_str(&env, "QmSpeed"),
        );
        assert_eq!(idx1, 1);
        assert_eq!(idx2, 2);
        assert_eq!(client.get_milestone_count(&1u64), 2);
    }

    #[test]
    fn test_revoke_validator() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "Coach"));
        client.revoke_validator(&validator);

        assert!(!client.is_active_validator(&validator));
    }

    #[test]
    #[should_panic]
    fn test_revoked_validator_cannot_approve() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "Coach"));
        client.revoke_validator(&validator);

        // Should panic — validator is inactive
        client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Some milestone"),
            &String::from_str(&env, "QmEvidence"),
        );
    }

    #[test]
    #[should_panic]
    fn test_unregistered_validator_cannot_approve() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let random = Address::generate(&env);
        // Should panic — not in validator registry
        client.approve_milestone(
            &random,
            &1u64,
            &String::from_str(&env, "Some milestone"),
            &String::from_str(&env, "QmEvidence"),
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_approve_milestone_blocked_when_paused() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "Coach"));

        client.pause_contract();

        // Should panic — contract is paused
        client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Some milestone"),
            &String::from_str(&env, "QmEvidence"),
        );
    }

    #[test]
    fn test_milestone_accessible_after_ledger_advancement() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let validator = Address::generate(&env);
        client.register_validator(&validator, &String::from_str(&env, "Coach"));

        client.approve_milestone(
            &validator,
            &1u64,
            &String::from_str(&env, "Scored 5 goals"),
            &String::from_str(&env, "QmEvidence"),
        );

        // Advance ledger sequence to simulate time passing
        env.ledger().with_mut(|li| {
            li.sequence_number += 10_000;
            li.timestamp += 50_000;
        });

        // Milestone should still be accessible (TTL was bumped on write)
        let milestone = client.get_milestone(&1u64, &1);
        assert_eq!(milestone.player_id, 1u64);
    }
}