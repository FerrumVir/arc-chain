//! What a node may adopt from a certified checkpoint (C11, design v2:
//! docs/design/checkpoint-rejoin.md).
//!
//! The certificate authenticates the tip block and the state root. This
//! module decides which parts of a payload those two commitments actually
//! cover, and returns only those parts; everything else must already be
//! identical to this node's own state, or adoption is refused.
//!
//! * History: the tip is certified. Every earlier window block is authenticated
//!   by hash linkage from the tip, with each hash **recomputed**, its map key
//!   equal to its header height, and its `tx_root` equal to the Merkle root of
//!   its `tx_hashes`. Transaction bodies, receipts and event logs are not bound
//!   to anything the certificate covers closely enough to adopt (a body's hash
//!   excludes its signature), so none of them is adopted.
//! * State, and only on a protocol-4 chain (native inference activated, no
//!   recovery context): accounts are covered by the account-only root. A
//!   storage row a certified account commits (a native escrow's metadata, the
//!   context pin) is adopted only if that account commits it - equality with
//!   this node's copy is not enough, since that copy may be the stale one.
//!   Every other storage row, and contracts, identities, validators, the
//!   staking pool and the reward activation height, must be byte-identical to
//!   this node's own: on protocol 4 nothing but native inference runs after
//!   activation, so none of them can change. Collections must be strictly
//!   ordered by key, so a payload cannot carry two versions of one row.
//! * Any other chain is refused. On a legacy account-only chain the uncovered
//!   domains do change (contract storage, stake), so "equal to this node's"
//!   would adopt stale state; a recovery-context chain's engine is
//!   repositioned only by its signed recovery path.
//! * Derived indexes (native pending, Tier 1, bond releases) are never taken
//!   from a payload; they are rebuilt from the adopted state.

use std::collections::{BTreeMap, BTreeSet};

use arc_crypto::{Hash256, MerkleTree};
use arc_types::Block;

use crate::snapshot::SnapshotPayload;
use crate::{StateDB, StateError};

fn refuse(reason: impl Into<String>) -> StateError {
    StateError::PersistenceError(format!("checkpoint refused: {}", reason.into()))
}

/// Verify the history window from the certified tip down. Returns the set of
/// transaction hashes the verified blocks list.
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

/// Accounts, storage holders and each holder's rows strictly increasing by
/// key: one version of anything. Installation keeps the last copy of a
/// duplicated key, and a stale copy placed after a checked one would
/// otherwise be what the node ends up with.
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

/// Every account filed under its own address. The account-only root hashes
/// each account's contents in key order but not the key itself, so an
/// account moved to another key between the same neighbours keeps the root
/// while its owner vanishes. Honest state never files an account elsewhere.
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

/// The adoption preconditions that depend only on this node and the payload,
/// not on the certified tip. Checked when planning and again immediately
/// before the durable write.
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
    // Without D20 a node that adopted a checkpoint, and so lacks the
    // receipts of already-applied transactions, could count candidates its
    // peers skip and select a different transaction.
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

/// Validators at or above the minimum stake, sorted. Activation records only
/// those, while every start re-seeds the full genesis list, so entries below
/// the minimum depend on when a node last restarted and carry no meaning.
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
    /// The part of `payload` this node may adopt, given the certified `tip`.
    /// The caller has verified the envelope (committee, domain, digest) and
    /// the tip against the certificate and the anchor commitment.
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
        adoption_preconditions(self, payload)?;

        verify_history_window(&payload.blocks, tip)?;

        // Domains nothing certified: they must already be this node's own.
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

        // Storage: equal to ours, or committed by a certified account.
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
        // Omission: a certified account whose storage root is new or changed
        // commits a row the payload must carry. Without it this node would
        // later refuse a transition its peers apply, which stops its
        // consensus loop.
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
