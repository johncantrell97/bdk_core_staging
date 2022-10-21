use bdk_core::*;
use bitcoin::{hashes::Hash, Txid};

fn gen_hash<H: Hash>(n: u64) -> H {
    let data = n.to_le_bytes();
    Hash::hash(&data[..])
}

fn gen_block_id(height: u32, hash_n: u64) -> BlockId {
    BlockId {
        height,
        hash: gen_hash(hash_n),
    }
}

#[test]
fn check_last_valid_rules() {
    let mut chain = SparseChain::default();

    chain
        .apply_update(Update::new(None, gen_block_id(0, 0)))
        .expect("add first tip should succeed");

    chain
        .apply_update(Update::new(Some(gen_block_id(0, 0)), gen_block_id(1, 1)))
        .expect("applying second tip on top of first should succeed");

    assert_eq!(
        chain.apply_update(Update::new(None, gen_block_id(2, 2))),
        Result::Err(StaleReason::UnexpectedLastValid {
            got: None,
            expected: Some(gen_block_id(1, 1))
        }),
        "applying third tip on top without specifying last valid should fail",
    );

    assert_eq!(
        chain.apply_update(Update::new(Some(gen_block_id(1, 2)), gen_block_id(3, 3),)),
        Result::Err(StaleReason::UnexpectedLastValid {
            got: Some(gen_block_id(1, 2)),
            expected: Some(gen_block_id(1, 1)),
        }),
        "applying new tip, in which suppled last_valid is non-existant, should fail",
    );

    assert_eq!(
        chain.apply_update(Update::new(Some(gen_block_id(1, 1)), gen_block_id(1, 3),)),
        Result::Err(StaleReason::LastValidConflictsNewTip {
            last_valid: gen_block_id(1, 1),
            new_tip: gen_block_id(1, 3),
        }),
        "applying new tip, in which new_tip conflicts last_valid, should fail",
    );

    assert_eq!(
        chain.apply_update(Update::new(Some(gen_block_id(1, 1)), gen_block_id(0, 3),)),
        Result::Err(StaleReason::LastValidConflictsNewTip {
            last_valid: gen_block_id(1, 1),
            new_tip: gen_block_id(0, 3),
        }),
        "applying new tip, in which new_tip conflicts last_valid, should fail (2)",
    );
}

#[test]
fn check_invalidate_rules() {
    let mut chain = SparseChain::default();

    // add one checkpoint
    chain
        .apply_update(Update::new(None, gen_block_id(1, 1)))
        .expect("should succeed");

    // when we are invalidating the one and only checkpoint, `last_valid` should be `None`
    assert_eq!(
        chain.apply_update(Update {
            invalidate: Some(gen_block_id(1, 1)),
            ..Update::new(Some(gen_block_id(1, 1)), gen_block_id(1, 2))
        }),
        Result::Err(StaleReason::UnexpectedLastValid {
            got: Some(gen_block_id(1, 1)),
            expected: None,
        }),
        "should fail when invalidate does not directly preceed last_valid",
    );
    assert_eq!(
        chain.apply_update(Update {
            invalidate: Some(gen_block_id(1, 1)),
            ..Update::new(None, gen_block_id(1, 2))
        }),
        Result::Ok(()),
        "invalidate should succeed",
    );

    // add two checkpoints
    assert_eq!(
        chain.apply_update(Update::new(Some(gen_block_id(1, 2)), gen_block_id(2, 3))),
        Result::Ok(())
    );
    assert_eq!(
        chain.apply_update(Update::new(Some(gen_block_id(2, 3)), gen_block_id(3, 4),)),
        Result::Ok(())
    );

    // `invalidate` should directly follow `last_valid`
    assert_eq!(
        chain.apply_update(Update {
            invalidate: Some(gen_block_id(3, 4)),
            ..Update::new(Some(gen_block_id(1, 2)), gen_block_id(3, 5))
        }),
        Result::Err(StaleReason::UnexpectedLastValid {
            got: Some(gen_block_id(1, 2)),
            expected: Some(gen_block_id(2, 3)),
        }),
        "should fail when checkpoint directly following last_valid is not invalidate",
    );
    assert_eq!(
        chain.apply_update(Update {
            invalidate: Some(gen_block_id(3, 4)),
            ..Update::new(Some(gen_block_id(2, 3)), gen_block_id(3, 5))
        }),
        Result::Ok(()),
        "should succeed",
    );
}

#[test]
fn apply_tips() {
    let mut chain = SparseChain::default();

    // gen 10 checkpoints
    let mut last_valid = None;
    for i in 0..10 {
        let new_tip = gen_block_id(i, i as _);
        assert_eq!(
            chain.apply_update(Update::new(last_valid, new_tip)),
            Result::Ok(()),
        );
        last_valid = Some(new_tip);
    }

    // repeated last tip should succeed
    assert_eq!(
        chain.apply_update(Update::new(last_valid, last_valid.unwrap())),
        Result::Ok(()),
        "repeated last_tip should succeed"
    );

    // ensure state of sparsechain is correct
    chain
        .iter_checkpoints(..)
        .zip(0..)
        .for_each(|(block_id, exp_height)| {
            assert_eq!(block_id, gen_block_id(exp_height, exp_height as _))
        });
}

#[test]
fn checkpoint_limit_is_respected() {
    let mut chain = SparseChain::default();
    chain.set_checkpoint_limit(Some(5));

    // gen 10 checkpoints
    let mut last_valid = None;
    for i in 0..10 {
        let new_tip = gen_block_id(i, i as _);
        assert_eq!(
            chain.apply_update(Update {
                txids: [(gen_hash(i as _), TxHeight::Confirmed(i))].into(),
                ..Update::new(last_valid, new_tip)
            }),
            Result::Ok(()),
        );
        last_valid = Some(new_tip);
    }

    assert_eq!(chain.iter_confirmed_txids().count(), 10);
    assert_eq!(chain.iter_checkpoints(..).count(), 5);
}

#[test]
fn add_txids() {
    let mut chain = SparseChain::default();

    let txids_1 = (0..100)
        .map(gen_hash::<Txid>)
        .map(|txid| (txid, TxHeight::Confirmed(1)))
        .collect();

    assert_eq!(
        chain.apply_update(Update {
            txids: txids_1,
            ..Update::new(None, gen_block_id(1, 1))
        }),
        Result::Ok(()),
        "add many txs in single checkpoint should succeed"
    );

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(2), TxHeight::Confirmed(3))]
                .into_iter()
                .collect(),
            ..Update::new(Some(gen_block_id(1, 1)), gen_block_id(2, 2))
        }),
        Result::Err(StaleReason::TxidHeightGreaterThanTip {
            new_tip: gen_block_id(2, 2),
            txid: (gen_hash(2), TxHeight::Confirmed(3)),
        }),
        "adding tx with height greater than new tip should fail",
    );
}

#[test]
fn add_txs_of_same_height_with_different_updates() {
    let mut chain = SparseChain::default();
    let block = gen_block_id(0, 0);

    // add one block
    assert_eq!(chain.apply_update(Update::new(None, block)), Result::Ok(()));

    // add txs of same height with different updates
    (0..100).for_each(|i| {
        assert_eq!(
            chain.apply_update(Update {
                txids: [(gen_hash(i as _), TxHeight::Confirmed(0))].into(),
                ..Update::new(Some(block), block)
            }),
            Result::Ok(()),
        );
    });

    assert_eq!(chain.iter_txids().count(), 100);
    assert_eq!(chain.iter_confirmed_txids().count(), 100);
    assert_eq!(chain.iter_mempool_txids().count(), 0);
    assert_eq!(chain.iter_checkpoints(..).count(), 1);
}

#[test]
fn confirm_tx() {
    let mut chain = SparseChain::default();

    assert_eq!(
        chain.apply_update(Update {
            txids: [
                (gen_hash(10), TxHeight::Unconfirmed),
                (gen_hash(20), TxHeight::Unconfirmed)
            ]
            .into(),
            ..Update::new(None, gen_block_id(1, 1))
        }),
        Result::Ok(()),
        "adding two txs from mempool should succeed"
    );

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(10), TxHeight::Confirmed(0))].into(),
            ..Update::new(Some(gen_block_id(1, 1)), gen_block_id(1, 1))
        }),
        Result::Ok(()),
        "it should be okay to confirm tx into block before last_valid (partial sync)",
    );
    assert_eq!(chain.iter_txids().count(), 2);
    assert_eq!(chain.iter_confirmed_txids().count(), 1);
    assert_eq!(chain.iter_mempool_txids().count(), 1);

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(20), TxHeight::Confirmed(2))].into(),
            ..Update::new(Some(gen_block_id(1, 1)), gen_block_id(2, 2))
        }),
        Result::Ok(()),
        "it should be okay to confirm tx into the tip introduced",
    );
    assert_eq!(chain.iter_txids().count(), 2);
    assert_eq!(chain.iter_confirmed_txids().count(), 2);
    assert_eq!(chain.iter_mempool_txids().count(), 0);

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(10), TxHeight::Unconfirmed)].into(),
            ..Update::new(Some(gen_block_id(2, 2)), gen_block_id(2, 2))
        }),
        Result::Err(StaleReason::TxUnexpectedlyMoved {
            txid: gen_hash(10),
            from: TxHeight::Confirmed(0),
            to: TxHeight::Unconfirmed,
        }),
        "tx cannot be unconfirmed without invalidate"
    );

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(20), TxHeight::Confirmed(3))].into(),
            ..Update::new(Some(gen_block_id(2, 2)), gen_block_id(3, 3))
        }),
        Result::Err(StaleReason::TxUnexpectedlyMoved {
            txid: gen_hash(20),
            from: TxHeight::Confirmed(2),
            to: TxHeight::Confirmed(3),
        }),
        "tx cannot move forward in blocks without invalidate"
    );

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(20), TxHeight::Confirmed(1))].into(),
            ..Update::new(Some(gen_block_id(2, 2)), gen_block_id(3, 3))
        }),
        Result::Err(StaleReason::TxUnexpectedlyMoved {
            txid: gen_hash(20),
            from: TxHeight::Confirmed(2),
            to: TxHeight::Confirmed(1),
        }),
        "tx cannot move backwards in blocks without invalidate"
    );

    assert_eq!(
        chain.apply_update(Update {
            txids: [(gen_hash(20), TxHeight::Confirmed(2))].into(),
            ..Update::new(Some(gen_block_id(2, 2)), gen_block_id(3, 3))
        }),
        Result::Ok(()),
        "update can introduce already-existing tx"
    );
    assert_eq!(chain.iter_txids().count(), 2);
    assert_eq!(chain.iter_confirmed_txids().count(), 2);
    assert_eq!(chain.iter_mempool_txids().count(), 0);
}
