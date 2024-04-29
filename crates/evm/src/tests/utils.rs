use lazy_static::lazy_static;
use reth_primitives::hex_literal::hex;
use reth_primitives::B256;
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::{Module, WorkingSet};
use sov_prover_storage_manager::{new_orphan_storage, SnapshotManager};
use sov_state::{DefaultStorageSpec, ProverStorage, Storage};

use crate::{Evm, EvmConfig};

type C = DefaultContext;

lazy_static! {
    pub(crate) static ref GENESIS_HASH: B256 = B256::from(hex!(
        "f02ef9e64296322b3bd217661431be41544f64750632daa02a07bd2c52b28656"
    ));
    pub(crate) static ref GENESIS_STATE_ROOT: B256 = B256::from(hex!(
        "1b49889efcbe36e1140122803727c5f6f5d12065461e3a6e2a4515f1d5ced9d5"
    ));
}

pub(crate) fn get_evm_with_storage(
    config: &EvmConfig,
) -> (
    Evm<C>,
    WorkingSet<DefaultContext>,
    ProverStorage<DefaultStorageSpec, SnapshotManager>,
) {
    let tmpdir = tempfile::tempdir().unwrap();
    let prover_storage = new_orphan_storage(tmpdir.path()).unwrap();
    let mut working_set = WorkingSet::new(prover_storage.clone());
    let evm = Evm::<C>::default();
    evm.genesis(config, &mut working_set).unwrap();

    let mut genesis_state_root = [0u8; 32];
    genesis_state_root.copy_from_slice(GENESIS_STATE_ROOT.as_ref());

    evm.finalize_hook(
        &genesis_state_root.into(),
        &mut working_set.accessory_state(),
    );
    (evm, working_set, prover_storage)
}

pub(crate) fn get_evm(config: &EvmConfig) -> (Evm<C>, WorkingSet<C>) {
    let tmpdir = tempfile::tempdir().unwrap();
    let storage = new_orphan_storage(tmpdir.path()).unwrap();
    let mut working_set = WorkingSet::new(storage.clone());
    let evm = Evm::<C>::default();
    evm.genesis(config, &mut working_set).unwrap();

    let root = commit(working_set, storage.clone());

    let mut working_set: WorkingSet<C> = WorkingSet::new(storage.clone());
    evm.finalize_hook(&root.into(), &mut working_set.accessory_state());

    evm.begin_soft_confirmation_hook([1u8; 32], 1, [2u8; 32], &root, 0, 0, &mut working_set);
    evm.end_soft_confirmation_hook(&mut working_set);

    let root = commit(working_set, storage.clone());
    let mut working_set: WorkingSet<C> = WorkingSet::new(storage.clone());
    evm.finalize_hook(&root.into(), &mut working_set.accessory_state());

    // let mut genesis_state_root = [0u8; 32];
    // genesis_state_root.copy_from_slice(GENESIS_STATE_ROOT.as_ref());

    (evm, working_set)
}

pub(crate) fn commit(
    working_set: WorkingSet<C>,
    storage: ProverStorage<DefaultStorageSpec, SnapshotManager>,
) -> [u8; 32] {
    // Save checkpoint
    let mut checkpoint = working_set.checkpoint();

    let (cache_log, witness) = checkpoint.freeze();

    let (root, authenticated_node_batch) = storage
        .compute_state_update(cache_log, &witness)
        .expect("jellyfish merkle tree update must succeed");

    let working_set = checkpoint.to_revertable();

    let accessory_log = working_set.checkpoint().freeze_non_provable();

    storage.commit(&authenticated_node_batch, &accessory_log);

    root.0
}