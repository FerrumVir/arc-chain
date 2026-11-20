use std::collections::{BTreeMap, BTreeSet};

use arc_crypto::{Hash256, MerkleTree};
use arc_types::Block;

use crate::snapshot::SnapshotPayload;
use crate::{StateDB, StateError};

fn refuse(reason: impl Into<String>) -> StateError {
    StateError::PersistenceError(format!("checkpoint refused: {}", reason.into()))
}

pub fn verify_history_window(
    blocks: &[(u64, Block)],
    tip: &Block,
) -> Result<BTreeSet<[u8; 32]>, StateError> {
    let Some((tip_height, carried_tip)) = blocks.last() else {
        return Err(refuse("the payload carries no tip block"));
    };
    if *tip_height != tip.header.height || carried_tip.hash != tip.hash {
        return Err(refuse("the payload's last block is not the certified tip"));
    }
    let mut listed = BTreeSet::new();
    let mut expected_hash = tip.hash;
    let mut expected_height = tip.header.height;
    for (index, (height, block)) in blocks.iter().enumerate().rev() {
        let recomputed = Block::compute_hash(&block.header);
        if *height != block.header.height
            || *height != expected_height
            || recomputed != block.hash
            || recomputed != expected_hash
        {
            return Err(refuse(format!(
                "window block at height {height} does not chain to the certified tip"
            )));
        }
        if block.header.tx_count as usize != block.tx_hashes.len()
            || MerkleTree::from_leaves(block.tx_hashes.clone()).root() != block.header.tx_root
        {
            return Err(refuse(format!(
                "window block at height {height} lists transactions its tx_root does not commit"
            )));
        }
        listed.extend(block.tx_hashes.iter().map(|hash| hash.0));
        if index > 0 {
            expected_hash = block.header.parent_hash;
            expected_height = height
                .checked_sub(1)
                .ok_or_else(|| refuse("window height underflow"))?;
        }
    }
    Ok(listed)
}

fn strictly_increasing(keys: impl IntoIterator<Item = [u8; 32]>) -> bool {
    let mut previous: Option<[u8; 32]> = None;
    for key in keys {
        if previous.is_some_and(|before| before >= key) {
            return false;
        }
        previous = Some(key);
    }
    true
}

fn strictly_ordered(payload: &SnapshotPayload) -> Result<(), StateError> {
    if !strictly_increasing(payload.accounts.iter().map(|(address, _)| address.0))
        || !strictly_increasing(payload.storage.iter().map(|(address, _)| address.0))
        || !payload
            .storage
            .iter()
            .all(|(_, rows)| strictly_increasing(rows.iter().map(|(key, _)| key.0)))
    {
        return Err(refuse(
            "its accounts or storage are not strictly ordered by key",
        ));
    }
    Ok(())
}

pub(crate) fn accounts_bound_to_keys(payload: &SnapshotPayload) -> Result<(), StateError> {
    match payload
        .accounts
        .iter()
        .find(|(key, account)| account.address != *key)
    {
        Some((key, account)) => Err(refuse(format!(
            "it files the account of {} under {key}, which the certified root does not bind",
            account.address
        ))),
        None => Ok(()),
    }
}

pub(crate) fn adoption_preconditions(
    state: &StateDB,
    payload: &SnapshotPayload,
) -> Result<(), StateError> {
    if state.gpu_cache.is_some() || state.use_jmt {
        return Err(refuse(
            "a GPU account cache or JMT-backed root cannot be rebuilt by a rebase",
        ));
    }
    if state.recovery_context().is_some() || payload.recovery_context.is_some() {
        return Err(refuse(
            "recovery-domain chains are repositioned only by their signed recovery path",
        ));
    }
    let Some(context) = state.native_inference_context() else {
        return Err(refuse(
            "only a protocol-4 chain's uncovered domains are frozen after genesis; on this \
             chain they can change, so a checkpoint cannot be adopted",
        ));
    };
    if context.selection_rule
        != crate::inference_contract_state::NativeSelectionRule::SkipUsedNoncesV2
    {
        return Err(refuse(
            "this chain was activated with selection rule v1, under which a rebased node can \
             select differently from its peers; only v2 chains adopt checkpoints",
        ));
    }
    strictly_ordered(payload)?;
    accounts_bound_to_keys(payload)
}

fn active_validator_list(
    validators: &[(arc_types::Address, u64)],
) -> Vec<(arc_types::Address, u64)> {
    let mut active: Vec<_> = validators
        .iter()
        .filter(|(_, stake)| *stake >= StateDB::MIN_VALIDATOR_STAKE)
        .copied()
        .collect();
    active.sort_by_key(|(address, _)| address.0);
    active
}

fn canonical<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StateError> {
    bincode::serialize(value).map_err(|error| refuse(format!("encode for comparison: {error}")))
}

impl StateDB {
    pub fn plan_checkpoint_adoption(
        &self,
        payload: &SnapshotPayload,
        tip: &Block,
    ) -> Result<SnapshotPayload, StateError> {
        if payload.height != tip.header.height || Block::compute_hash(&tip.header) != tip.hash {
            return Err(refuse("the tip does not describe the checkpoint height"));
        }
        if payload.height <= self.height() {
            return Err(refuse("the checkpoint does not move this node forward"));
        }
        verify_history_window(&payload.blocks, tip)?;

        if let Some((lowest_height, lowest)) = payload.blocks.first() {
            for (height, block) in &payload.blocks {
                if let Some(ours) = self.get_block(*height)
                    && ours.hash != block.hash
                {
                    return Err(refuse(format!(
                        "window block at height {height} is not the block this node committed \
                         there"
                    )));
                }
            }
            if let Some(parent_height) = lowest_height.checked_sub(1)
                && let Some(ours) = self.get_block(parent_height)
                && ours.hash != lowest.header.parent_hash
            {
                return Err(refuse(format!(
                    "the window starting at {lowest_height} does not extend this node's block at \
                     {parent_height}"
                )));
            }
        }

        adoption_preconditions(self, payload)?;

        let local = self.export_durable_snapshot();
        for (name, theirs, ours) in [
            (
                "contracts",
                canonical(&payload.contracts)?,
                canonical(&local.contracts)?,
            ),
            (
                "identities",
                canonical(&payload.identities)?,
                canonical(&local.identities)?,
            ),
            (
                "active validators",
                canonical(&active_validator_list(&payload.validators))?,
                canonical(&active_validator_list(&local.validators))?,
            ),
            (
                "staking pool",
                canonical(&payload.staking_pool)?,
                canonical(&local.staking_pool)?,
            ),
            (
                "community-reward activation height",
                canonical(&payload.community_rewards_activation_height)?,
                canonical(&local.community_rewards_activation_height)?,
            ),
        ] {
            if theirs != ours {
                return Err(refuse(format!(
                    "its {name} differ from this node's, and the account-only state root does not \
                     cover them"
                )));
            }
        }

        let accounts: BTreeMap<[u8; 32], &arc_types::Account> = payload
            .accounts
            .iter()
            .map(|(address, account)| (address.0, account))
            .collect();
        let ours: BTreeMap<([u8; 32], [u8; 32]), &Vec<u8>> = local
            .storage
            .iter()
            .flat_map(|(address, rows)| {
                rows.iter()
                    .map(move |(key, value)| ((address.0, key.0), value))
            })
            .collect();
        let mut theirs = BTreeSet::new();
        for (address, rows) in &payload.storage {
            for (key, value) in rows {
                theirs.insert((address.0, key.0));
                let accepted = if crate::inference_contract_state::native_storage_row_is_committable(
                    address, key,
                ) {
                    crate::inference_contract_state::native_storage_row_committed(
                        accounts.get(&address.0).copied(),
                        address,
                        key,
                        value,
                    )
                } else {
                    ours.get(&(address.0, key.0))
                        .is_some_and(|mine| *mine == value)
                };
                if !accepted {
                    return Err(refuse(format!(
                        "storage row {key} of {address} is neither committed by its certified \
                         account nor, where no account commits it, identical to this node's"
                    )));
                }
            }
        }
        let local_roots: BTreeMap<[u8; 32], Hash256> = local
            .accounts
            .iter()
            .map(|(address, account)| (address.0, account.storage_root))
            .collect();
        for (address, account) in &payload.accounts {
            if account.storage_root == Hash256::ZERO
                || local_roots.get(&address.0) == Some(&account.storage_root)
            {
                continue;
            }
            let carried = payload
                .storage
                .iter()
                .filter(|(holder, _)| holder == address)
                .flat_map(|(_, rows)| rows.iter())
                .any(|(key, value)| {
                    crate::inference_contract_state::native_storage_row_committed(
                        Some(account),
                        address,
                        key,
                        value,
                    )
                });
            if !carried {
                return Err(refuse(format!(
                    "certified account {address} commits storage the checkpoint does not carry"
                )));
            }
        }
        if let Some(((address, key), _)) = ours.iter().find(|(slot, _)| !theirs.contains(*slot)) {
            return Err(refuse(format!(
                "it would delete storage row {} of {}, which nothing certified",
                Hash256(*key),
                Hash256(*address)
            )));
        }

        let mut adopted = SnapshotPayload {
            height: payload.height,
            accounts: payload.accounts.clone(),
            storage: payload.storage.clone(),
            contracts: local.contracts,
            identities: local.identities,
            blocks: payload.blocks.clone(),
            receipts: Vec::new(),
            full_transactions: Vec::new(),
            event_logs: Vec::new(),
            validators: local.validators,
            staking_pool: local.staking_pool,
            recovery_context: None,
            community_rewards_activation_height: local.community_rewards_activation_height,
            native_inference_pending: Vec::new(),
        };
        adopted.canonicalize();
        Ok(adopted)
    }
}
