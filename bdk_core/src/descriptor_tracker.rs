use crate::{BlockTime, CheckPoint, HashMap, HashSet, PrevOuts};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};
use bitcoin::{
    psbt::{self, PartiallySignedTransaction as Psbt},
    secp256k1::{Secp256k1, VerifyOnly},
    util::address::WitnessVersion,
    BlockHash, OutPoint, Script, Transaction, TxIn, TxOut, Txid,
};
use miniscript::{
    descriptor::DerivedDescriptorKey, psbt::PsbtInputExt, Descriptor, DescriptorPublicKey,
};

#[derive(Clone, Debug)]
pub struct DescriptorTracker {
    /// The descriptor we are tracking
    descriptor: Descriptor<DescriptorPublicKey>,
    /// Which txids are included in which checkpoints
    checkpointed_txs: BTreeMap<u32, (BlockHash, HashSet<Txid>)>,
    /// Index the outpoints owned by this tracker to the derivation index of script pubkey.
    txouts: BTreeMap<OutPoint, u32>,
    /// Which tx spent each output. This indexes the spends for every transaction regardless if the
    /// outpoint has one of our scripts pubkeys.
    spends: BTreeMap<OutPoint, (u32, Txid)>,
    /// The unspent txouts
    unspent: HashSet<OutPoint>,
    /// The ordered script pubkeys that have been derived from the descriptor.
    scripts: Vec<Script>,
    /// A reverse lookup from script to derivation index
    script_indexes: HashMap<Script, u32>,
    /// A lookup from script pubkey to outpoint
    script_txouts: BTreeMap<u32, HashSet<OutPoint>>,
    /// A set of script derivation indexes that haven't been spent to
    unused: BTreeSet<u32>,
    /// Map from txid to metadata
    txs: HashMap<Txid, AugmentedTx>,
    /// Index of transactions that are in the mempool
    mempool: HashSet<Txid>,
    // TODO change to blocktime + height
    // Optionally we need the consensus time i.e. Median time past
    // https://github.com/bitcoin/bitcoin/blob/a4e066af8573dcefb11dff120e1c09e8cf7f40c2/src/chain.h#L290-L302
    latest_blockheight: Option<u32>,
    secp: Secp256k1<VerifyOnly>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateResult {
    /// The update was applied successfully.
    // TODO: return a diff
    Ok,
    /// The update cannot be applied to the current state because it does not apply to the current
    /// tip of the tracker or does not invalidate the right checkpoint such that it does.
    Stale,
    /// The update you tried to apply was inconsistent with the current state.
    ///
    /// To resolve the consistency you can treat the update as invalid and ignore it or invalidate
    /// the checkpoint and re-apply it.
    Inconsistent {
        txid: Txid,
        conflicts_with: Txid,
        at_checkpoint: CheckPoint,
    },
}

impl DescriptorTracker {
    pub fn new(descriptor: Descriptor<DescriptorPublicKey>) -> Self {
        Self {
            descriptor,
            checkpointed_txs: Default::default(),
            secp: Secp256k1::verification_only(),
            txouts: Default::default(),
            spends: Default::default(),
            unspent: Default::default(),
            scripts: Default::default(),
            script_indexes: Default::default(),
            script_txouts: Default::default(),
            unused: Default::default(),
            txs: Default::default(),
            mempool: Default::default(),
            latest_blockheight: Default::default(),
        }
    }

    pub fn latest_blockheight(&self) -> Option<u32> {
        self.latest_blockheight
    }

    pub fn descriptor(&self) -> &Descriptor<DescriptorPublicKey> {
        &self.descriptor
    }

    pub fn next_derivation_index(&self) -> u32 {
        self.scripts.len() as u32
    }

    pub fn latest_checkpoint(&self) -> Option<CheckPoint> {
        self.checkpointed_txs
            .iter()
            .last()
            .map(|(height, (hash, _))| CheckPoint {
                height: *height,
                hash: *hash,
            })
    }

    pub fn checkpoint_at(&self, height: u32) -> Option<CheckPoint> {
        self.checkpointed_txs
            .get(&height)
            .map(|(hash, _)| CheckPoint {
                height,
                hash: *hash,
            })
    }

    fn best_checkpoint_for(&self, height: u32) -> Option<CheckPoint> {
        self.checkpointed_txs
            .range(height..)
            .map(|(height, (hash, _))| CheckPoint {
                height: *height,
                hash: *hash,
            })
            .next()
    }

    pub fn iter_checkpoints(&self) -> impl Iterator<Item = (CheckPoint, &HashSet<Txid>)> {
        self.checkpointed_txs.iter().map(|(height, (hash, txs))| {
            (
                CheckPoint {
                    height: *height,
                    hash: *hash,
                },
                txs,
            )
        })
    }

    fn remove_tx(&mut self, txid: Txid) {
        let aug_tx = match self.txs.remove(&txid) {
            Some(aug_tx) => aug_tx,
            None => {
                debug_assert!(!self.mempool.contains(&txid));
                return;
            }
        };
        for input in &aug_tx.tx.input {
            if let Some((_, tx_that_spends)) = self.spends.remove(&input.previous_output) {
                debug_assert_eq!(
                    tx_that_spends, txid,
                    "the one that spent it must be this one"
                );
            }

            if self.txouts.contains_key(&input.previous_output) {
                self.unspent.insert(input.previous_output);
            }
        }

        for i in 0..aug_tx.tx.output.len() {
            let txout_to_remove = OutPoint {
                vout: i as u32,
                txid,
            };
            if let Some(derivation_index) = self.txouts.remove(&txout_to_remove) {
                self.script_txouts
                    .get_mut(&derivation_index)
                    .expect("guaranteed to exist")
                    .remove(&txout_to_remove);
            }
        }

        self.mempool.remove(&txid);
    }

    fn add_tx(&mut self, inputs: PrevOuts, tx: Transaction, confirmation_time: Option<BlockTime>) {
        let txid = tx.txid();

        let inputs_sum = match inputs {
            PrevOuts::Coinbase => {
                debug_assert_eq!(tx.input.len(), 1);
                debug_assert!(tx.input[0].previous_output.is_null());
                // HACK: set to 0. We only use this for fee which for coinbase is always 0.
                0
            }
            PrevOuts::Spend(txouts) => txouts.iter().map(|input| input.value).sum(),
        };
        let outputs_sum: u64 = tx.output.iter().map(|out| out.value).sum();
        // we need to saturating sub since we want coinbase txs to map to 0 fee and
        // this subtraction will be negative for coinbase txs.
        let fee = inputs_sum.saturating_sub(outputs_sum);
        let feerate = fee as f32 / tx.weight() as f32;

        let conflicts = tx
            .input
            .iter()
            .filter_map(|input| {
                self.spends
                    .get(&input.previous_output)
                    .map(|(_, txid)| *txid)
            })
            .collect::<Vec<_>>();

        if confirmation_time.is_some() {
            // the only things we conflict with are in the mempool and this is confirmed so delete it
            for conflicting_txid in conflicts {
                self.remove_tx(conflicting_txid);
            }
        } else {
            // NOTE: We have already made sure that all conflicts are unconfirmed. Therefore
            // TODO: Make resolution for mempool conflicts customizable
            let conflicing_tx_with_higher_feerate = conflicts.iter().find(|conflicting_txid| {
                self.txs.get(*conflicting_txid).expect("must exist").feerate > feerate
            });
            if conflicing_tx_with_higher_feerate.is_none() {
                for conflicting_txid in conflicts {
                    self.remove_tx(conflicting_txid);
                }
            } else {
                // we shouldn't add this tx as it conflicts with one with a higher feerate.
                return;
            }
        }

        for (i, input) in tx.input.iter().enumerate() {
            let removed = self.spends.insert(input.previous_output, (i as u32, txid));
            debug_assert_eq!(
                removed, None,
                "we should have already removed all conflicts!"
            );
            self.unspent.remove(&input.previous_output);
        }

        for (i, out) in tx.output.iter().enumerate() {
            if let Some(index) = self.index_of_derived_script(&out.script_pubkey) {
                let outpoint = OutPoint {
                    txid,
                    vout: i as u32,
                };

                self.txouts.insert(outpoint, index);

                if !self.spends.contains_key(&outpoint) {
                    self.unspent.insert(outpoint);
                }

                let txos_for_script = self.script_txouts.entry(index).or_default();
                txos_for_script.insert(outpoint);
                self.unused.remove(&index);
            }
        }

        match confirmation_time {
            Some(confirmation_time) => {
                // Find the first checkpoint above or equal to the tx's height
                let checkpoint_height: Option<u32> = self
                    .checkpointed_txs
                    .range(confirmation_time.height..)
                    .next()
                    .map(|(height, _)| *height);

                match checkpoint_height {
                    Some(checkpoint_height) => {
                        // Rebase onto the checkpoint, removing all checkpoints after after
                        // and including the target. We do this to keep the rule: Never add new txs
                        // to a checkpoint once you've added it into checkpointed_txs. But we *can*
                        // remove checkpoints and move the txids from older ones to the tip.
                        //
                        // NOTE: the usual case is that checkpoint_height == tip_height in which
                        // case the following will just insert the new txid into the tip.
                        let removed = self.checkpointed_txs.split_off(&checkpoint_height);
                        let (tip_height, (tip_hash, _)) = removed.iter().rev().next().unwrap();
                        let txids = removed
                            .values()
                            .map(|(_, txs)| txs.iter().cloned())
                            .flatten()
                            .chain(core::iter::once(txid))
                            .collect();
                        self.checkpointed_txs
                            .insert(*tip_height, (*tip_hash, txids));
                        self.mempool.remove(&txid);
                    }
                    None => {
                        unreachable!(
                            "the caller must have checked that no txs are outside of range"
                        )
                    }
                }
            }
            None => {
                self.mempool.insert(txid);
            }
        }

        self.txs.insert(
            txid,
            AugmentedTx {
                tx,
                fee,
                feerate,
                confirmation_time,
            },
        );
    }

    fn invalidate_checkpoint(&mut self, height: u32) {
        let removed = self.checkpointed_txs.split_off(&height);
        let txs_to_remove = removed.values().map(|(_, txs)| txs).flatten();
        for tx_to_remove in txs_to_remove {
            self.remove_tx(*tx_to_remove);
        }
    }

    pub fn apply_update(&mut self, update: Update) -> UpdateResult {
        // Do consistency checks first so we don't mutate anything until we're sure the update is
        // valid.
        for (_, tx, confirmation_time) in &update.transactions {
            let txid = tx.txid();
            if let Some(existing) = self.txs.get(&tx.txid()) {
                if let Some(existing_time) = existing.confirmation_time {
                    if confirmation_time != &Some(existing_time) {
                        let at_checkpoint = self
                            .best_checkpoint_for(existing_time.height)
                            .expect("must exist since there's a confirmed tx");
                        return UpdateResult::Inconsistent {
                            txid,
                            conflicts_with: existing.tx.txid(),
                            at_checkpoint,
                        };
                    }
                }
            }
            let conflicts = tx
                .input
                .iter()
                .filter_map(|input| self.spends.get(&input.previous_output));
            for (_, conflicting_txid) in conflicts {
                if let Some(conflicting_conftime) = self
                    .txs
                    .get(conflicting_txid)
                    .expect("must exist")
                    .confirmation_time
                {
                    let at_checkpoint = self
                        .best_checkpoint_for(conflicting_conftime.height)
                        .expect("must exist since there's a confirmed tx");
                    return UpdateResult::Inconsistent {
                        txid,
                        conflicts_with: *conflicting_txid,
                        at_checkpoint,
                    };
                }
            }
        }

        if let Some(last_active_index) = update.last_active_index {
            // It's possible that we find a script derived at a higher index than what we have given
            // out in the case where another system is deriving from the same descriptor.
            self.derive_scripts(last_active_index);
        }

        // look for invalidated and check that start tip is the one before it.
        match update.invalidate {
            Some(checkpoint_reset) => match self.checkpointed_txs.get(&checkpoint_reset.height) {
                Some((existing_hash, _)) => {
                    if *existing_hash != checkpoint_reset.hash {
                        if self
                            .checkpointed_txs
                            .range(..checkpoint_reset.height)
                            .last()
                            .map(|(height, (hash, _))| CheckPoint {
                                height: *height,
                                hash: *hash,
                            })
                            == update.base_tip
                        {
                            self.invalidate_checkpoint(checkpoint_reset.height);
                        } else {
                            return UpdateResult::Stale;
                        }
                    } else {
                        return UpdateResult::Stale;
                    }
                }
                None => return UpdateResult::Stale,
            },
            None => {
                if update.base_tip != self.latest_checkpoint() {
                    return UpdateResult::Stale;
                }
            }
        }

        if update.mempool_is_total_set {
            // This update will include everything in the mempool that is relevent to the tracker so
            // we clear everything.
            self.clear_mempool();
        }

        // Insert a new empty checkpoint at the update height
        self.checkpointed_txs
            .entry(update.new_tip.height)
            .or_insert_with(|| (update.new_tip.hash, Default::default()));

        for (vouts, tx, confirmation_time) in update.transactions {
            self.add_tx(vouts, tx, confirmation_time);
        }

        let (_, tip_txids) = self.checkpointed_txs.values().rev().next().unwrap();

        if tip_txids.is_empty() {
            // the new checkpoint we inserted ends up empty so delete it
            self.checkpointed_txs.remove(&update.new_tip.height);
        }

        self.latest_blockheight = Some(update.new_tip.height);

        UpdateResult::Ok
    }

    pub fn clear_mempool(&mut self) {
        let mempool = core::mem::replace(&mut self.mempool, Default::default());
        for txid in mempool {
            self.remove_tx(txid);
        }

        debug_assert!(self.mempool.is_empty())
    }

    pub fn disconnect_block(&mut self, block_height: u32, block_header: BlockHash) {
        // Can't guarantee that mempool is consistent with chain after we disconnect a block so we
        // clear it.
        // TODO: it would be nice if we could only delete those transactions that are
        // inconsistent by recording the latest block they were included in.
        self.clear_mempool();
        if let Some((existing_block_header, _)) = self.checkpointed_txs.get(&block_height) {
            if *existing_block_header == block_header {
                self.invalidate_checkpoint(block_height);
            }
        }
    }

    pub fn iter_tx(&self) -> impl Iterator<Item = (Txid, &AugmentedTx)> {
        self.txs.iter().map(|(txid, tx)| (*txid, tx))
    }

    pub fn iter_unspent(&self) -> impl Iterator<Item = LocalTxOut> + '_ {
        self.unspent
            .iter()
            .map(|txo| (txo, self.txouts.get(txo).expect("txout must exist")))
            .map(|(txo, index)| self.create_txout(*txo, *index))
    }

    fn create_txout(&self, outpoint: OutPoint, derivation_index: u32) -> LocalTxOut {
        let tx = self
            .txs
            .get(&outpoint.txid)
            .expect("must exist since we have the txout");
        let spent_by = self.spends.get(&outpoint).cloned();
        let value = self.txs.get(&outpoint.txid).expect("must exist").tx.output
            [outpoint.vout as usize]
            .value;
        LocalTxOut {
            value,
            spent_by,
            outpoint,
            derivation_index,
            confirmed_at: tx.confirmation_time,
        }
    }

    pub fn iter_txout(&self) -> impl Iterator<Item = LocalTxOut> + '_ {
        self.txouts
            .iter()
            .map(|(outpoint, data)| self.create_txout(*outpoint, *data))
    }

    pub fn get_txout(&self, txo: OutPoint) -> Option<LocalTxOut> {
        let data = self.txouts.get(&txo)?;
        Some(self.create_txout(txo, *data))
    }

    pub fn get_tx(&self, txid: Txid) -> Option<&AugmentedTx> {
        self.txs.get(&txid)
    }

    /// Iterates over all the script pubkeys of a descriptor.
    ///
    /// This method does **not** use the tracker's stored scripts and returned iterator does not
    /// hold a reference to the tracker. This allows it to be sent between threads. If the
    /// descriptor `is_deriveable` then the iterator will derive and emit all non-hardened indexes
    /// of the descriptor otherwise it will just have one script in it.
    ///
    /// **WARNING**: never turn these into addresses or send coins to them.
    /// The tracker may not be able to find them.
    /// To get a script you can use as an address use [`derive_next`].
    ///
    /// [`derive_next`]: Self::derive_next
    pub fn iter_all_scripts(&self) -> impl Iterator<Item = Script> + Send {
        let descriptor = self.descriptor.clone();
        let end = if self.descriptor.is_deriveable() {
            // Because we only iterate over non-hardened indexes there are 2^31 values
            (1 << 31) - 1
        } else {
            0
        };

        let secp = self.secp.clone();
        (0..=end).map(move |i| {
            descriptor
                .derive(i)
                .derived_descriptor(&secp)
                .expect("the descritpor cannot need hardened derivation")
                .script_pubkey()
        })
    }

    /// Returns the script that has been derived at the index.
    ///
    /// If that index hasn't been derived yet it will return None instead.
    pub fn script_at_index(&self, index: u32) -> Option<&Script> {
        self.scripts.get(index as usize)
    }

    /// Derives a new script pubkey which can be turned into an address.
    ///
    /// The tracker returns a new address for each call to this method and stores it internally so
    /// it will be able to find transactions related to it.
    pub fn derive_new(&mut self) -> (u32, &Script) {
        let next_derivation_index = if self.descriptor.is_deriveable() {
            0
        } else {
            self.scripts.len() as u32
        };
        self.derive_scripts(next_derivation_index);
        let script = self
            .scripts
            .get(next_derivation_index as usize)
            .expect("we just derived to that index");
        (next_derivation_index, script)
    }

    /// Derives and stores a new address only if we haven't already got one that hasn't been used
    /// yet.
    pub fn derive_next_unused(&mut self) -> (u32, &Script) {
        let need_new = self.iter_unused_scripts().next().is_none();
        // this rather strange branch is needed because of some lifetime issues
        if need_new {
            self.derive_new()
        } else {
            self.iter_unused_scripts().next().unwrap()
        }
    }

    /// Iterate over the scripts that have been derived already
    pub fn iter_scripts(&self) -> impl Iterator<Item = &Script> {
        self.scripts.iter()
    }

    /// Iterate over the scripts that have been derived but we have not seen a transaction spending
    /// from it.
    pub fn iter_unused_scripts(&self) -> impl Iterator<Item = (u32, &Script)> {
        self.unused
            .iter()
            .map(|index| (*index, self.script_at_index(*index).expect("must exist")))
    }

    /// Returns whether the script at index `index` has been used or not.
    ///
    /// Will also return `false` if the script at `index` hasn't been derived yet (because we have
    /// no way of knowing if it has been used yet in that case).
    pub fn is_used(&self, index: u32) -> bool {
        !self.unused.contains(&index) && (index as usize) < self.scripts.len()
    }

    /// Derives and stores all the scripts **up to and including** `end`.
    ///
    /// Returns whether any new scripts needed deriving.
    pub fn derive_scripts(&mut self, end: u32) -> bool {
        let end = match self.descriptor.is_deriveable() {
            false => 0,
            true => end,
        };

        let needed = (end + 1).saturating_sub(self.scripts.len() as u32);
        for index in self.scripts.len()..self.scripts.len() + needed as usize {
            let script = self
                .descriptor
                .derive(index as u32)
                .derived_descriptor(&self.secp)
                .expect("the descritpor cannot need hardened derivation")
                .script_pubkey();
            self.scripts.push(script.clone());
            self.script_indexes.insert(script.clone(), index as u32);
            self.unused.insert(index as u32);
        }

        needed == 0
    }

    /// Returns at what derivation index a script pubkey was derived at.
    pub fn index_of_derived_script(&self, script: &Script) -> Option<u32> {
        self.script_indexes.get(script).cloned()
    }

    /// The maximum satisfaction weight of a descriptor
    pub fn max_satisfaction_weight(&self) -> u32 {
        self.descriptor
            .derive(0)
            .max_satisfaction_weight()
            .expect("descriptor is well formed") as u32
    }

    /// The dust value for any script used as a script pubkey on the network.
    ///
    /// Transactions with output containing script pubkeys from this descriptor with values below
    /// this will not be relayed by the network.
    pub fn dust_value(&self) -> u64 {
        self.descriptor
            .derive(0)
            .script_pubkey()
            .dust_value()
            .as_sat()
    }

    /// Prepare an input for insertion into a PSBT
    pub fn prime_input(&self, op: OutPoint) -> Option<PrimedInput> {
        let derivation_index = self.txouts.get(&op)?;
        let descriptor = self.descriptor().derive(*derivation_index);
        let mut psbt_input = psbt::Input::default();

        let prev_tx = self
            .txs
            .get(&op.txid)
            .expect("since the txout exists so mus the transaction");

        match self.descriptor().desc_type().segwit_version() {
            Some(version) => {
                if version < WitnessVersion::V1 {
                    psbt_input.non_witness_utxo = Some(prev_tx.tx.clone());
                }
                psbt_input.witness_utxo = Some(prev_tx.tx.output[op.vout as usize].clone());
            }
            None => psbt_input.non_witness_utxo = Some(prev_tx.tx.clone()),
        }

        psbt_input
            .update_with_descriptor_unchecked(&descriptor)
            .expect("conversion error cannot happen if descriptor is well formed");

        let primed_input = PrimedInput {
            descriptor,
            psbt_input,
        };

        Some(primed_input)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub transactions: Vec<(PrevOuts, Transaction, Option<BlockTime>)>,
    pub mempool_is_total_set: bool,
    pub last_active_index: Option<u32>,
    /// The data in the update can be applied upon this checkpoint. If None then it is not
    /// consistent with any particular tip (apart from new tip) and so should form the base
    pub base_tip: Option<CheckPoint>,
    /// Invalidates a particular checkpoint
    pub invalidate: Option<CheckPoint>,
    /// The data is valid with respect to new_tip
    pub new_tip: CheckPoint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AugmentedTx {
    pub tx: Transaction,
    pub fee: u64,
    pub feerate: f32,
    pub confirmation_time: Option<BlockTime>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTxOut {
    pub value: u64,
    pub spent_by: Option<(u32, Txid)>,
    pub outpoint: OutPoint,
    pub derivation_index: u32,
    pub confirmed_at: Option<BlockTime>,
}

pub trait MultiTracker {
    fn iter_unspent(&self) -> Box<dyn Iterator<Item = (usize, LocalTxOut)> + '_>;
    fn iter_txout(&self) -> Box<dyn Iterator<Item = (usize, LocalTxOut)> + '_>;
    fn latest_blockheight(&self) -> Option<u32>;
    fn create_psbt<I, O>(
        &self,
        inputs: I,
        outputs: O,
    ) -> (Psbt, BTreeMap<usize, Descriptor<DerivedDescriptorKey>>)
    where
        I: IntoIterator<Item = OutPoint>,
        O: IntoIterator<Item = TxOut>;
}

impl<'a> MultiTracker for [DescriptorTracker] {
    fn iter_unspent(&self) -> Box<dyn Iterator<Item = (usize, LocalTxOut)> + '_> {
        Box::new(
            self.into_iter()
                .enumerate()
                .flat_map(|(i, tracker)| tracker.iter_unspent().map(move |u| (i, u))),
        )
    }

    fn iter_txout(&self) -> Box<dyn Iterator<Item = (usize, LocalTxOut)> + '_> {
        Box::new(
            self.into_iter()
                .enumerate()
                .flat_map(|(i, tracker)| tracker.iter_txout().map(move |u| (i, u))),
        )
    }

    fn latest_blockheight(&self) -> Option<u32> {
        self.into_iter()
            .filter_map(|tracker| tracker.latest_blockheight())
            .max()
    }

    fn create_psbt<I, O>(
        &self,
        inputs: I,
        outputs: O,
    ) -> (Psbt, BTreeMap<usize, Descriptor<DerivedDescriptorKey>>)
    where
        I: IntoIterator<Item = OutPoint>,
        O: IntoIterator<Item = TxOut>,
    {
        let unsigned_tx = Transaction {
            version: 0x01,
            lock_time: 0x00,
            input: inputs
                .into_iter()
                .map(|previous_output| TxIn {
                    previous_output,
                    ..Default::default()
                })
                .collect(),
            output: outputs.into_iter().collect(),
        };

        let mut descriptors = BTreeMap::new();

        let mut psbt = Psbt::from_unsigned_tx(unsigned_tx).unwrap();

        for ((input_index, psbt_input), txin) in psbt
            .inputs
            .iter_mut()
            .enumerate()
            .zip(&psbt.unsigned_tx.input)
        {
            if let Some(primed_input) = self
                .iter()
                .find_map(|tracker| tracker.prime_input(txin.previous_output))
            {
                *psbt_input = primed_input.psbt_input;
                descriptors.insert(input_index, primed_input.descriptor);
            }
        }

        (psbt, descriptors)
    }
}

#[derive(Debug, Clone)]
pub struct PrimedInput {
    pub descriptor: Descriptor<DerivedDescriptorKey>,
    pub psbt_input: psbt::Input,
}

#[cfg(test)]
mod test {
    use bitcoin::{BlockHash, Transaction, TxIn, TxOut};
    use core::cmp::max;

    use super::*;

    const DESCRIPTOR: &'static str = "wpkh(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL)";

    pub enum IOSpec {
        Mine(u64, usize),
        Other(u64),
    }

    pub struct TxSpec {
        inputs: Vec<IOSpec>,
        outputs: Vec<IOSpec>,
        confirmed_at: Option<u32>,
        is_coinbase: bool,
    }

    fn create_update(scripts: Vec<Script>, txs: Vec<TxSpec>, checkpoint_height: u32) -> Update {
        let last_active_index = txs.iter().fold(None, |lai, tx_spec| {
            tx_spec
                .inputs
                .iter()
                .chain(tx_spec.outputs.iter())
                .fold(lai, |lai, spec| match (lai, spec) {
                    (Some(lai), IOSpec::Mine(_, index)) => Some(max(*index as u32, lai)),
                    (None, IOSpec::Mine(_, index)) => Some(*index as u32),
                    _ => lai,
                })
        });
        let transactions = txs
            .into_iter()
            .map(|tx_spec| {
                (
                    match tx_spec.is_coinbase {
                        false => PrevOuts::Spend(
                            tx_spec
                                .inputs
                                .iter()
                                .map(|in_spec| match in_spec {
                                    IOSpec::Mine(value, index) => TxOut {
                                        value: *value,
                                        script_pubkey: scripts[*index].clone(),
                                    },
                                    IOSpec::Other(value) => TxOut {
                                        value: *value,
                                        script_pubkey: Default::default(),
                                    },
                                })
                                .collect(),
                        ),
                        true => {
                            todo!()
                        }
                    },
                    Transaction {
                        version: 1,
                        lock_time: 0,
                        input: if tx_spec.is_coinbase {
                            todo!()
                        } else {
                            tx_spec.inputs.iter().map(|_| TxIn::default()).collect()
                        },
                        output: tx_spec
                            .outputs
                            .into_iter()
                            .map(|out_spec| match out_spec {
                                IOSpec::Other(value) => TxOut {
                                    value,
                                    script_pubkey: Script::default(),
                                },
                                IOSpec::Mine(value, index) => TxOut {
                                    value,
                                    script_pubkey: scripts[index].clone(),
                                },
                            })
                            .collect(),
                    },
                    tx_spec.confirmed_at.map(|confirmed_at| BlockTime {
                        height: confirmed_at,
                        time: confirmed_at as u64,
                    }),
                )
            })
            .collect();

        Update {
            transactions,
            last_active_index,
            new_tip: CheckPoint {
                height: checkpoint_height,
                hash: BlockHash::default(),
            },
            invalidate: None,
            mempool_is_total_set: true,
            base_tip: None,
        }
    }

    #[test]
    fn apply_update_no_checkpoint() {
        let mut tracker = DescriptorTracker::new(DESCRIPTOR.parse().unwrap());
        let scripts = tracker.iter_all_scripts().take(5).collect::<Vec<_>>();
        use IOSpec::*;

        let mut update = create_update(
            scripts,
            vec![TxSpec {
                inputs: vec![Other(2_000)],
                outputs: vec![Mine(1_000, 0), Other(1_800)],
                confirmed_at: None,
                is_coinbase: false,
            }],
            0,
        );

        assert_eq!(tracker.apply_update(update.clone()), UpdateResult::Ok);

        let txouts = tracker.iter_txout().collect::<Vec<_>>();
        let txs = tracker.iter_tx().collect::<Vec<_>>();
        let unspent = tracker.iter_unspent().collect::<Vec<_>>();
        let checkpoints = tracker.iter_checkpoints().collect::<Vec<_>>();
        assert_eq!(txouts.len(), 1);
        assert_eq!(unspent, txouts);
        assert_eq!(txs.len(), 1);
        assert_eq!(checkpoints.len(), 0);
        assert_eq!(txouts.len(), 1);

        update.transactions[0].2 = Some(BlockTime { height: 1, time: 1 });
        update.new_tip = CheckPoint {
            height: update.new_tip.height + 1,
            hash: update.new_tip.hash,
        };

        assert_eq!(tracker.apply_update(update), UpdateResult::Ok);

        let txs = tracker.iter_tx().collect::<Vec<_>>();
        let checkpoints = tracker.iter_checkpoints().collect::<Vec<_>>();
        let txouts = tracker.iter_txout().collect::<Vec<_>>();

        assert_eq!(checkpoints.len(), 1);
        assert_eq!(txouts.len(), 1);
        assert_eq!(
            checkpoints[0].1,
            &txs.into_iter().map(|(x, _)| x).collect::<HashSet<_>>()
        );
    }
}
