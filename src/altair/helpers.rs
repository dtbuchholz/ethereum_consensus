use crate::altair as spec;

use crate::crypto::{eth_aggregate_pubkeys, hash};
use crate::domains::DomainType;
use crate::primitives::{Epoch, Gwei, ParticipationFlags, ValidatorIndex};
use crate::state_transition::{
    invalid_operation_error, Context, Error, InvalidAttestation, InvalidOperation, Result,
};
use integer_sqrt::IntegerSquareRoot;
use spec::{
    compute_shuffled_index, decrease_balance, get_active_validator_indices,
    get_beacon_proposer_index, get_block_root, get_block_root_at_slot, get_current_epoch,
    get_eligible_validator_indices, get_previous_epoch, get_seed, get_total_active_balance,
    get_total_balance, increase_balance, initiate_validator_exit, is_in_inactivity_leak,
    sync::SyncCommittee, AttestationData, BeaconState,
};
use ssz_rs::Vector;
use std::collections::HashSet;

pub fn add_flag(flags: ParticipationFlags, flag_index: u8) -> ParticipationFlags {
    // Return a new ``ParticipationFlags`` adding ``flag_index`` to ``flags``
    let flag = 2u8.pow(flag_index as u32);
    flags | flag
}

pub fn has_flag(flags: ParticipationFlags, flag_index: u8) -> bool {
    // Return whether ``flags`` has ``flag_index`` set
    let flag = 2u8.pow(flag_index as u32);
    flags & flag == flag
}

pub fn get_next_sync_committee_indices<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    context: &Context,
) -> Result<Vec<ValidatorIndex>> {
    // Return the sync committee indices, with possible duplicates, for the next sync committee.
    let epoch = get_current_epoch(state, context) + 1;
    let max_random_byte = u8::MAX as u64;
    let active_validator_indices = get_active_validator_indices(state, epoch);
    let active_validator_count = active_validator_indices.len();
    let seed = get_seed(state, epoch, DomainType::SyncCommittee, context);
    let i = 0;
    let mut sync_committee_indices = Vec::<ValidatorIndex>::new();
    // @dev need to validate this is correct for `hash`
    let mut hash_input = [0u8; 40];
    hash_input[..32].copy_from_slice(seed.as_ref());
    while sync_committee_indices.len() < context.sync_committee_size {
        let shuffled_index = compute_shuffled_index(
            i % active_validator_count,
            active_validator_count,
            &seed,
            context,
        )?;
        let candidate_index = active_validator_indices[shuffled_index];

        let i_bytes: [u8; 8] = (i / 32).to_le_bytes();
        hash_input[32..].copy_from_slice(&i_bytes);
        let random_byte = hash(hash_input).as_ref()[(i % 32)] as u64;
        let effective_balance = state.validators[candidate_index].effective_balance;

        if effective_balance * max_random_byte >= context.max_effective_balance * random_byte {
            sync_committee_indices.push(candidate_index);
        }
    }
    Ok(sync_committee_indices)
}

pub fn get_next_sync_committee<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    context: &Context,
) -> Result<SyncCommittee<SYNC_COMMITTEE_SIZE>> {
    // Return the next sync committee, with possible pubkey duplicates.
    let indices = get_next_sync_committee_indices(state, context)?;
    let public_keys = state
        .validators
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            if indices.contains(&i) {
                Some(&v.public_key)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    // @dev not sure if the impl of `eth_aggregate_pubkeys` is needed or correct
    // also, not sure if `expect` is desired here
    let aggregate_public_key = eth_aggregate_pubkeys(&public_keys)
        .expect("validator public_keys should be aggregated into aggregate_public_key");

    // @dev ran into trouble here so just cloned() the validator's public_key
    // couldn't figure out how to properly coerce/convert `public_keys` Vec to ssz_rs::Vector
    let pks_vector =
        Vector::<_, SYNC_COMMITTEE_SIZE>::from_iter(public_keys.iter().map(|&pk| pk.clone()));

    Ok(SyncCommittee::<SYNC_COMMITTEE_SIZE> {
        public_keys: pks_vector,
        aggregate_public_key,
    })
}

pub fn get_base_reward_per_increment<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    context: &Context,
) -> Result<Gwei> {
    Ok(
        context.effective_balance_increment * context.base_reward_factor
            / get_total_active_balance(state, context)?.integer_sqrt(),
    )
}

pub fn get_base_reward<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    index: ValidatorIndex,
    context: &Context,
) -> Result<Gwei> {
    // Return the base reward for the validator defined by ``index`` with respect to the current `state`
    let increments =
        state.validators[index].effective_balance / context.effective_balance_increment;
    Ok(increments * get_base_reward_per_increment(state, context)?)
}

pub fn get_unslashed_participating_indices<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    flag_index: usize,
    epoch: Epoch,
    context: &Context,
) -> Result<HashSet<ValidatorIndex>> {
    // Return the set of validator indices that are both active and unslashed for the given ``flag_index`` and ``epoch``
    let previous_epoch = get_previous_epoch(state, context);
    let current_epoch = get_current_epoch(state, context);
    if previous_epoch != epoch && current_epoch != epoch {
        return Err(Error::InvalidEpoch {
            requested: epoch,
            previous: previous_epoch,
            current: current_epoch,
        });
    }

    let epoch_participation = if epoch == current_epoch {
        &state.current_epoch_participation
    } else {
        &state.previous_epoch_participation
    };
    let active_validator_indices = get_active_validator_indices(state, epoch);
    let participating_indices = active_validator_indices
        .iter()
        .filter_map(|&i| {
            if has_flag(epoch_participation[i], flag_index as u8) {
                Some(i)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let unslashed_particpating_indices = state
        .validators
        .iter()
        .enumerate()
        .filter_map(|(i, v)| if !v.slashed { Some(i) } else { None })
        .filter(|i| participating_indices.contains(i))
        .collect::<HashSet<_>>();
    Ok(unslashed_particpating_indices)
}

pub fn get_attestation_participation_flag_indices<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    data: &AttestationData,
    inclusion_delay: u64,
    context: &Context,
) -> Result<Vec<usize>> {
    // Return the flag indices that are satisfied by an attestation.
    let justified_checkpoint = if data.target.epoch == get_current_epoch(state, context) {
        &state.current_justified_checkpoint
    } else {
        &state.previous_justified_checkpoint
    };

    // Matching roots
    let is_matching_source = data.source == *justified_checkpoint;
    if !is_matching_source {
        return Err(invalid_operation_error(InvalidOperation::Attestation(
            InvalidAttestation::InvalidSource {
                expected: justified_checkpoint.clone(),
                source_checkpoint: data.source.clone(),
                current: get_current_epoch(state, context),
            },
        )));
    }
    let is_matching_target = is_matching_source
        && (data.target.root == *get_block_root(state, data.target.epoch, context)?);
    let is_matching_head = is_matching_target
        && (data.beacon_block_root == *get_block_root_at_slot(state, data.slot)?);

    let mut participation_flag_indices = Vec::new();
    if is_matching_source && inclusion_delay <= context.slots_per_epoch.integer_sqrt() {
        participation_flag_indices.push(crate::altair::TIMELY_SOURCE_FLAG_INDEX);
    }
    if is_matching_target && inclusion_delay <= context.slots_per_epoch {
        participation_flag_indices.push(crate::altair::TIMELY_TARGET_FLAG_INDEX);
    }
    if is_matching_head && inclusion_delay == context.min_attestation_inclusion_delay {
        participation_flag_indices.push(crate::altair::TIMELY_HEAD_FLAG_INDEX);
    }

    Ok(participation_flag_indices)
}

pub fn get_flag_index_deltas<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    flag_index: usize,
    context: &Context,
) -> Result<(Vec<Gwei>, Vec<Gwei>)> {
    // Return the deltas for a given ``flag_index`` by scanning through the participation flags.
    let validator_count = state.validators.len();
    let mut rewards = vec![0; validator_count];
    let mut penalties = vec![0; validator_count];
    let previous_epoch = get_previous_epoch(state, context);
    let unslashed_participating_indices =
        get_unslashed_participating_indices(state, flag_index, previous_epoch, context)?;
    let weight = crate::altair::PARTICIPATION_FLAG_WEIGHTS[flag_index];
    let unslashed_participating_balance =
        get_total_balance(state, &unslashed_participating_indices, context)?;
    let unslashed_participating_increments =
        unslashed_participating_balance / context.effective_balance_increment;
    let active_increments =
        get_total_active_balance(state, context)? / context.effective_balance_increment;
    for index in get_eligible_validator_indices(state, context) {
        let base_reward = get_base_reward(state, index, context)?;
        if unslashed_participating_indices.contains(&index) {
            if !is_in_inactivity_leak(state, context) {
                let reward_numerator = base_reward * weight * unslashed_participating_increments;
                rewards[index] +=
                    reward_numerator / (active_increments * crate::altair::WEIGHT_DENOMINATOR);
            } else if flag_index != crate::altair::TIMELY_HEAD_FLAG_INDEX {
                penalties[index] += base_reward * weight / crate::altair::WEIGHT_DENOMINATOR;
            }
        }
    }
    Ok((rewards, penalties))
}

pub fn get_inactivity_penalty_deltas<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    context: &Context,
) -> Result<(Vec<Gwei>, Vec<Gwei>)> {
    let validator_count = state.validators.len();
    let rewards = vec![0; validator_count];
    let mut penalties = vec![0; validator_count];
    let previous_epoch = get_previous_epoch(state, context);
    // NOTE: direct imports to simplify forward code gen of these constants
    let matching_target_indices = get_unslashed_participating_indices(
        state,
        crate::altair::TIMELY_TARGET_FLAG_INDEX,
        previous_epoch,
        context,
    )?;
    let current_epoch = get_current_epoch(state, context);
    let inactivity_penalty_quotient = context.inactivity_penalty_quotient(current_epoch)?;
    for i in get_eligible_validator_indices(state, context) {
        if !matching_target_indices.contains(&i) {
            let penalty_numerator =
                state.validators[i].effective_balance * state.inactivity_scores[i];
            let penalty_denominator = context.inactivity_score_bias * inactivity_penalty_quotient;
            penalties[i] += penalty_numerator / penalty_denominator;
        }
    }
    Ok((rewards, penalties))
}

pub fn slash_validator<
    const SLOTS_PER_HISTORICAL_ROOT: usize,
    const HISTORICAL_ROOTS_LIMIT: usize,
    const ETH1_DATA_VOTES_BOUND: usize,
    const VALIDATOR_REGISTRY_LIMIT: usize,
    const EPOCHS_PER_HISTORICAL_VECTOR: usize,
    const EPOCHS_PER_SLASHINGS_VECTOR: usize,
    const MAX_VALIDATORS_PER_COMMITTEE: usize,
    const SYNC_COMMITTEE_SIZE: usize,
>(
    state: &mut BeaconState<
        SLOTS_PER_HISTORICAL_ROOT,
        HISTORICAL_ROOTS_LIMIT,
        ETH1_DATA_VOTES_BOUND,
        VALIDATOR_REGISTRY_LIMIT,
        EPOCHS_PER_HISTORICAL_VECTOR,
        EPOCHS_PER_SLASHINGS_VECTOR,
        MAX_VALIDATORS_PER_COMMITTEE,
        SYNC_COMMITTEE_SIZE,
    >,
    slashed_index: ValidatorIndex,
    whistleblower_index: Option<ValidatorIndex>,
    context: &Context,
) -> Result<()> {
    let epoch = get_current_epoch(state, context);
    initiate_validator_exit(state, slashed_index, context);
    state.validators[slashed_index].slashed = true;
    state.validators[slashed_index].withdrawable_epoch = u64::max(
        state.validators[slashed_index].withdrawable_epoch,
        epoch + context.epochs_per_slashings_vector as u64,
    );
    let slashings_index = epoch as usize % EPOCHS_PER_SLASHINGS_VECTOR;
    state.slashings[slashings_index] += state.validators[slashed_index].effective_balance;
    let min_slashing_penalty_quotient = context.min_slashing_penalty_quotient(epoch)?;
    decrease_balance(
        state,
        slashed_index,
        state.validators[slashed_index].effective_balance / min_slashing_penalty_quotient,
    );

    let proposer_index = get_beacon_proposer_index(state, context)?;

    let whistleblower_index = whistleblower_index.unwrap_or(proposer_index);

    let whistleblower_reward =
        state.validators[slashed_index].effective_balance / context.whistleblower_reward_quotient;
    // NOTE: direct imports to simplify forward code gen of these constants
    let proposer_reward_scaling_factor =
        crate::altair::PROPOSER_WEIGHT / crate::altair::WEIGHT_DENOMINATOR;
    let proposer_reward = whistleblower_reward * proposer_reward_scaling_factor;
    increase_balance(state, proposer_index, proposer_reward);
    increase_balance(
        state,
        whistleblower_index,
        whistleblower_reward - proposer_reward,
    );
    Ok(())
}