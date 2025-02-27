use citrea_fullnode::CitreaFullnode;
use sov_mock_da::{
    MockAddress, MockBlob, MockBlock, MockBlockHeader, MockDaConfig, MockDaService, MockDaSpec,
    MockValidityCond, PlannedFork,
};
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::default_context::DefaultContext;
use sov_stf_runner::{
    FullNodeConfig, InitVariant, RollupPublicKeys, RpcConfig, RunnerConfig, StorageConfig,
};

mod hash_stf;

use hash_stf::{get_result_from_blocks, HashStf, Q, S};
use sov_db::ledger_db::{LedgerDB, NodeLedgerOps};
use sov_mock_zkvm::MockCodeCommitment;
use sov_prover_storage_manager::ProverStorageManager;
use sov_rollup_interface::services::da::DaService;
use sov_rollup_interface::storage::HierarchicalStorageManager;
use sov_state::storage::NativeStorage;
use sov_state::{ProverStorage, Storage};
use tokio::sync::broadcast;

type MockInitVariant =
    InitVariant<HashStf<MockValidityCond>, MockZkvm<MockValidityCond>, MockDaSpec>;
#[tokio::test]
#[ignore]
async fn test_simple_reorg_case() {
    let tmpdir = tempfile::tempdir().unwrap();
    let sequencer_address = MockAddress::new([11u8; 32]);
    let genesis_params = vec![1, 2, 3, 4, 5];

    let main_chain_blobs = vec![
        vec![1, 1, 1, 1],
        vec![2, 2, 2, 2],
        vec![3, 3, 3, 3],
        vec![4, 4, 4, 4],
    ];
    let fork_blobs = vec![
        vec![13, 13, 13, 13],
        vec![14, 14, 14, 14],
        vec![15, 15, 15, 15],
    ];
    let expected_final_blobs = vec![
        vec![1, 1, 1, 1],
        vec![2, 2, 2, 2],
        vec![13, 13, 13, 13],
        vec![14, 14, 14, 14],
        vec![15, 15, 15, 15],
    ];

    let mut da_service = MockDaService::with_finality(sequencer_address, 4, tmpdir.path());
    da_service.set_wait_attempts(2);

    let _genesis_header = da_service.get_last_finalized_block_header().await.unwrap();

    let planned_fork = PlannedFork::new(5, 2, fork_blobs.clone());
    da_service.set_planned_fork(planned_fork).await.unwrap();

    for b in &main_chain_blobs {
        da_service.send_transaction(b).await.unwrap();
    }

    let (expected_state_root, _expected_final_root_hash) =
        get_expected_execution_hash_from(&genesis_params, expected_final_blobs);
    let (_expected_committed_state_root, expected_committed_root_hash) =
        get_expected_execution_hash_from(&genesis_params, vec![vec![1, 1, 1, 1]]);

    let init_variant: MockInitVariant = InitVariant::Genesis(genesis_params);

    let (before, after) = runner_execution(tmpdir.path(), init_variant, da_service).await;
    assert_ne!(before, after);
    assert_eq!(expected_state_root, after);

    let committed_root_hash = get_saved_root_hash(tmpdir.path()).unwrap().unwrap();

    assert_eq!(expected_committed_root_hash.unwrap(), committed_root_hash);
}

#[tokio::test]
#[ignore = "TBD"]
async fn test_several_reorgs() {}

#[tokio::test]
#[ignore]
async fn test_instant_finality_data_stored() {
    let tmpdir = tempfile::tempdir().unwrap();
    let sequencer_address = MockAddress::new([11u8; 32]);
    let genesis_params = vec![1, 2, 3, 4, 5];

    let mut da_service = MockDaService::new(sequencer_address, tmpdir.path());
    da_service.set_wait_attempts(2);

    let _genesis_header = da_service.get_last_finalized_block_header().await.unwrap();

    da_service.send_transaction(&[1, 1, 1, 1]).await.unwrap();
    da_service.send_transaction(&[2, 2, 2, 2]).await.unwrap();
    da_service.send_transaction(&[3, 3, 3, 3]).await.unwrap();

    let (expected_state_root, expected_root_hash) = get_expected_execution_hash_from(
        &genesis_params,
        vec![vec![1, 1, 1, 1], vec![2, 2, 2, 2], vec![3, 3, 3, 3]],
    );

    let init_variant: MockInitVariant = InitVariant::Genesis(genesis_params);

    let (before, after) = runner_execution(tmpdir.path(), init_variant, da_service).await;
    assert_ne!(before, after);
    assert_eq!(expected_state_root, after);

    let saved_root_hash = get_saved_root_hash(tmpdir.path()).unwrap().unwrap();

    assert_eq!(expected_root_hash.unwrap(), saved_root_hash);
}

async fn runner_execution(
    storage_path: &std::path::Path,
    init_variant: MockInitVariant,
    da_service: MockDaService,
) -> ([u8; 32], [u8; 32]) {
    let rollup_storage_path = storage_path.join("rollup").to_path_buf();
    let rollup_config = FullNodeConfig::<MockDaConfig> {
        storage: StorageConfig {
            path: rollup_storage_path.clone(),
        },
        rpc: RpcConfig {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            max_connections: 1024,
            max_request_body_size: 10 * 1024 * 1024,
            max_response_body_size: 10 * 1024 * 1024,
            batch_requests_limit: 50,
            enable_subscriptions: true,
            max_subscriptions_per_connection: 100,
        },
        runner: Some(RunnerConfig {
            sequencer_client_url: "http://127.0.0.1:4444".to_string(),
            include_tx_body: true,
            accept_public_input_as_proven: None,
        }),
        da: MockDaConfig {
            sender_address: da_service.get_sequencer_address(),
            db_path: storage_path.join("da").to_path_buf(),
        },
        public_keys: RollupPublicKeys {
            sequencer_public_key: vec![0u8; 32],
            sequencer_da_pub_key: vec![],
            prover_da_pub_key: vec![],
        },
        sync_blocks_count: 10,
    };

    let ledger_db = LedgerDB::with_path(rollup_storage_path.clone()).unwrap();

    let stf = HashStf::<MockValidityCond>::new();

    let storage_config = sov_state::config::Config {
        path: rollup_storage_path,
    };
    let storage_manager = ProverStorageManager::new(storage_config).unwrap();

    let mut runner: CitreaFullnode<_, _, _, _, DefaultContext, _> = CitreaFullnode::new(
        rollup_config.runner.unwrap(),
        rollup_config.public_keys,
        rollup_config.rpc,
        da_service,
        ledger_db,
        stf,
        storage_manager,
        init_variant,
        MockCodeCommitment([1u8; 32]),
        10,
        broadcast::channel(1).0,
    )
    .unwrap();

    let before = *runner.get_state_root();
    let end = runner.run().await;
    assert!(end.is_err());
    let after = *runner.get_state_root();

    (before, after)
}

fn get_saved_root_hash(
    path: &std::path::Path,
) -> anyhow::Result<Option<<ProverStorage<S, Q> as Storage>::Root>> {
    let storage_config = sov_state::config::Config {
        path: path.to_path_buf(),
    };
    let mut storage_manager = ProverStorageManager::<MockDaSpec, S>::new(storage_config).unwrap();
    let finalized_storage = storage_manager.create_finalized_storage()?;

    let ledger_db = LedgerDB::with_path(path).unwrap();

    ledger_db
        .get_head_slot()?
        .map(|(number, _)| finalized_storage.get_root_hash(number.0))
        .transpose()
}

fn get_expected_execution_hash_from(
    genesis_params: &[u8],
    blobs: Vec<Vec<u8>>,
) -> ([u8; 32], Option<<ProverStorage<S, Q> as Storage>::Root>) {
    let blocks: Vec<MockBlock> = blobs
        .into_iter()
        .enumerate()
        .map(|(idx, blob)| MockBlock {
            header: MockBlockHeader::from_height((idx + 1) as u64),
            validity_cond: MockValidityCond::default(),
            blobs: vec![MockBlob::new(
                blob,
                MockAddress::new([11u8; 32]),
                [idx as u8; 32],
            )],
        })
        .collect();

    get_result_from_blocks(genesis_params, &blocks[..])
}
