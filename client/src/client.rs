use crate::client::Error::ForkChoiceUpdated;
use crate::config::{AppConfig, CliArgs};
use crate::data::{self, Database, Lib};
use crate::execution_api_client::{ExecutionApiClient, ExecutionApiError, RpcRequest};
use crate::json_rpc::JsonResponseBody;
use alloy_rpc_types::Block;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use eyre::{Context, Result};
use reth_primitives::revm_primitives::bitvec::macros::internal::funty::Fundamental;
use reth_primitives::B256;
use serde_json::json;
use telos_translator_rs::block::TelosEVMBlock;
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tracing::{debug, error, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // #[error("Failed to sync block info.")]
    // BlockSyncInfo,
    // #[error("Executor block past config stop block.")]
    // ExecutorBlockPastStopBlock,
    // #[error("Latest block not found.")]
    // LatestBlockNotFound,
    #[error("Cannot start consensus client {0}")]
    CannotStartConsensusClient(String),
    // #[error("Spawn translator error")]
    // SpawnTranslator,
    #[error("Executor hash mismatch.")]
    ExecutorHashMismatch,
    #[error("Fork choice updated error: {0}")]
    ForkChoiceUpdated(String),
    #[error("New payload error: {0}")]
    NewPayloadV1(String),
    #[error("Database error: {0}")]
    Database(eyre::Report),
    #[error("Client is too many blocks ({0}) behind the executor, start from a more recent block or increase maximum range"
    )]
    RangeAboveMaximum(u32),
    #[error("Cannot shutdown translator: {0}")]
    TranslatorShutdown(String),
    #[error("Call to execution API failed: {0}")]
    ExecutionApiError(#[from] ExecutionApiError),
    #[error("Failed to run consensus client: {0}")]
    ConsensusClientRun(#[from] JoinError),
}

const SAFE_HASH_LOOKUP: u32 = 50;

pub struct Shutdown(mpsc::Sender<()>);
impl Shutdown {
    #[allow(dead_code)]
    pub async fn shutdown(&self) -> Result<()> {
        Ok(self.0.send(()).await?)
    }
}

pub struct ConsensusClient {
    pub config: AppConfig,
    execution_api: ExecutionApiClient,
    //latest_consensus_block: ExecutionPayloadV1,
    pub latest_executor_block: Option<Block>,
    pub latest_finalized_executor_block: Option<Block>,
    //is_forked: bool,
    pub db: Database,
    shutdown_tx: mpsc::Sender<()>,
    shutdown_rx: mpsc::Receiver<()>,
}

impl ConsensusClient {
    pub async fn new(args: &CliArgs, config: AppConfig) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let execution_api = ExecutionApiClient::new(&config.execution_endpoint, &config.jwt_secret)
            .wrap_err("Failed to create Execution API client")?;

        let db = match args.clean {
            false => Database::open(&config.data_path)?,
            true => Database::init(&config.data_path)?,
        };
        let latest_executor_block = execution_api
            .get_latest_block()
            .await
            .wrap_err("Failed to get latest executor block")?;
        let latest_finalized_executor_block = execution_api
            .get_latest_finalized_block()
            .await
            .wrap_err("Failed to get latest valid executor block")?;

        Ok(Self {
            config,
            execution_api,
            latest_executor_block,
            latest_finalized_executor_block,
            db,
            shutdown_tx,
            shutdown_rx,
        })
    }

    #[allow(dead_code)]
    pub fn shutdown_handle(&self) -> Shutdown {
        Shutdown(self.shutdown_tx.clone())
    }

    fn latest_evm_block(&self) -> Option<(u32, String)> {
        let latest = self.latest_finalized_executor_block.as_ref()?;
        let (number, hash) = (latest.header.number, latest.header.hash);
        Some((number.as_u32(), hash.to_string()))
    }

    pub fn is_in_start_stop_range(&self, block: u32) -> bool {
        match (self.config.evm_start_block, self.config.evm_stop_block) {
            (start_block, Some(stop_block)) => start_block <= block && block <= stop_block,
            (start_block, None) => start_block <= block,
        }
    }

    pub fn is_in_check_range(&self, block: u64) -> bool {
        match (
            &self.latest_finalized_executor_block,
            &self.latest_executor_block,
        ) {
            (None, None) => false,
            (None, Some(latest)) => block < latest.header.number,
            (Some(_), None) => unreachable!(),
            (Some(valid), Some(latest)) => {
                valid.header.number <= block && block <= latest.header.number
            }
        }
    }

    pub fn latest_evm_number(&self) -> Option<u32> {
        self.latest_finalized_executor_block
            .as_ref()
            .map(|block| block.header.number.as_u32())
    }

    pub fn sync_range(&self) -> Option<u32> {
        self.latest_evm_number()?
            .checked_sub(self.config.evm_start_block)
    }

    pub async fn run(mut self, mut rx: mpsc::Receiver<TelosEVMBlock>) -> Result<(), Error> {
        let mut batch = vec![];
        let chain_id = &self.config.chain_id;
        let mut lib: data::Block = self.db.get_lib()?.unwrap_or_default();
        let mut last_lib_hash: Option<B256> = None;

        loop {
            let message = tokio::select! {
                message = rx.recv() => message,
                _ = self.shutdown_rx.recv() => {
                    debug!("Shutdown signal received");
                    break;
                }
            };

            let Some(block) = message else {
                break;
            };

            let block_num = block.block_num;

            // Fast skip: a block at or below reth's most recently finalized
            // (LIB) cannot fork — Savanna BFT guarantees finality is
            // irreversible. Avoids per-block RPC chatter during startup
            // catchup when SHIP redelivers everything from `evm_start_block`.
            if let Some((latest_finalized_num, _)) = self.latest_evm_block() {
                if block_num <= latest_finalized_num {
                    debug!(
                        "Block {block_num} skipped (<= reth finalized {latest_finalized_num})"
                    );
                    continue;
                }
            }

            // ---- Fork-handling MVP (must run BEFORE put_block) ------------
            // SHIP runs with `irreversible_only: false`, so it streams head
            // blocks. Antelope DPoS produces ~6 microforks/day on mainnet
            // (max observed depth: 7 blocks). Without explicit handling, a
            // microfork either (a) trips the legacy ExecutorHashMismatch check
            // and crashes the CL, or (b) silently lands as orphaned state in
            // reth's MDBX.
            //
            // This block detects three fork shapes against the CL's own DB
            // and, on detection, rewinds reth to LIB and clears the orphan
            // CL DB entries above LIB. SHIP will continue to stream the new
            // chain's blocks; subsequent loop iterations process them as
            // normal forward progress.
            //
            // Detection runs BEFORE put_block so cl_latest reflects the
            // previous-block state, not the block we just received.
            //
            // Three detection cases:
            //   1. Same-height fork: SHIP delivers block N where we already
            //      have a block at N with a different hash.
            //   2. Parent-mismatch: SHIP delivers block at our_latest+1 whose
            //      parent_hash differs from our latest's block_hash.
            //   3. Below-latest with different hash: SHIP delivers an older
            //      height where we have a different block (deeper reorg).
            //
            // After rewind, we fall through and process this block normally
            // (it now becomes the new canonical block at its height).
            // ---------------------------------------------------------------
            let cl_latest_opt = self.db.get_latest_block()?;
            if let Some(cl_latest) = &cl_latest_opt {
                let new_hash = block.block_hash.to_string();
                let new_parent = format!("{:?}", block.header.parent_hash);

                let mut fork_detected = false;
                let mut fork_kind = "";
                let mut already_processed = false;

                if block_num == cl_latest.number {
                    if hashes_equal(&new_hash, &cl_latest.hash) {
                        already_processed = true;
                    } else {
                        fork_detected = true;
                        fork_kind = "same-height";
                    }
                } else if block_num == cl_latest.number + 1
                    && !hashes_equal(&new_parent, &cl_latest.hash)
                {
                    fork_detected = true;
                    fork_kind = "parent-mismatch";
                } else if block_num < cl_latest.number {
                    if let Some(stored) = self.db.get_block_or_prev(block_num)? {
                        if stored.number == block_num {
                            if hashes_equal(&stored.hash, &new_hash) {
                                already_processed = true;
                            } else {
                                fork_detected = true;
                                fork_kind = "deep";
                            }
                        }
                    }
                }

                if already_processed {
                    debug!("Block {block_num} already in CL DB; skipping");
                    continue;
                }

                if fork_detected {
                    warn!(
                        "REORG ({fork_kind}) detected at block {} (cl_latest={}/{}, ship={}/{}, parent={})",
                        block_num,
                        cl_latest.number,
                        cl_latest.hash,
                        block_num,
                        new_hash,
                        new_parent,
                    );
                    self.handle_reorg(&lib, last_lib_hash).await?;
                    last_lib_hash = None;  // reset; will be re-set on next finalized block
                    // Refresh reth's finalized snapshot so subsequent
                    // calculations reflect the post-rewind state. Best-effort:
                    // a stale value here only affects logging.
                    if let Ok(refreshed) =
                        self.execution_api.get_latest_finalized_block().await
                    {
                        self.latest_finalized_executor_block = refreshed;
                    }
                    // Fall through and process the new (now-canonical) block.
                }
            }
            // ---- end fork-handling MVP ------------------------------------

            self.db.put_block(From::from(&block))?;
            debug!("Block {block_num} put in the database");

            let latest_start = block_num.saturating_sub(self.config.latest_blocks_in_db_num);

            // Keep latest blocks and every nth block
            if latest_start > 0 && latest_start % self.config.block_checkpoint_interval != 0 {
                self.db.delete_block(latest_start)?;
                debug!("Block {latest_start} deleted from the database");
            }

            // NOTE: Case when new lib < current one is not supported
            let is_new_lib = lib.number != block.lib_num;

            if is_new_lib {
                let new_lib = Lib(&block);
                self.db.put_lib(Lib(&block).into())?;
                info!("LIB {new_lib:?} put in the database");
                lib = new_lib.into();
            }

            let block_hash = block.block_hash;

            if self.is_in_check_range(block_num.as_u64()) {
                debug!("Checking if block {block_num} exists...");
                let evm_block = self
                    .execution_api
                    .get_block_by_number(block_num.into())
                    .await?;

                if let Some(evm_block) = evm_block {
                    if evm_block.header.hash != block_hash {
                        // Reth's canonical-headers index may be stale for this
                        // block (known reth db inconsistency that manifested
                        // after the Apr 2026 sampling-opt crash loop). If reth
                        // still has the correct block stored under its hash,
                        // treat this as recoverable: log and continue. A
                        // later newPayload/forkchoiceUpdated will re-canonicalize.
                        let block_hash_str = block_hash.to_string();
                        match self
                            .execution_api
                            .get_block_by_hash(&block_hash_str)
                            .await?
                        {
                            Some(_) => {
                                warn!(
                                    "CHECK-RANGE index inconsistency at block {}                                      (reth number->hash returns {:?} but block                                      {} is stored under hash in reth; continuing)",
                                    block_num, evm_block.header.hash, block_hash_str,
                                );
                                continue;
                            }
                            None => {
                                error!(
                                    "CHECK-RANGE MISMATCH at block {}: consensus_hash={:?} reth_stored_hash={:?} reth_parent_hash={:?} reth_extra_data={:?}",
                                    block_num,
                                    block_hash,
                                    evm_block.header.hash,
                                    evm_block.header.parent_hash,
                                    evm_block.header.extra_data,
                                );
                                return Err(Error::ExecutorHashMismatch);
                            }
                        }
                    }
                    continue;
                }
            }

            let block_is_final = block.is_final(chain_id);
            let block_is_lib = block.is_lib(chain_id);
            let lib_evm_num = block.lib_evm_num(chain_id);

            batch.push(block);

            // if LIB is less or equal than current block batch size is 1 or more blocks
            // if LIB is greater than current block send in batches
            let flush = !block_is_final || block_is_lib || batch.len() == self.config.batch_size;

            if !flush {
                continue;
            };

            let safe_hash = self
                .db
                .get_block_or_prev(block_num.saturating_sub(SAFE_HASH_LOOKUP))?
                .map(|block| block.hash.parse().unwrap())
                .unwrap_or(block_hash);

            // Telos test mode: zero finalized triggers optimistic sync in reth v1.11.3
            let finalized_hash = if block_is_final {
                debug!("Synced to head, LIB < current block");
                Some(block_hash)
            } else if is_new_lib {
                debug!("New LIB is detected");
                self.db
                    .get_block_or_prev(lib_evm_num)?
                    .map(|block| block.hash.parse().unwrap())
            } else {
                debug!("Synced to head, LIB is unchanged");
                last_lib_hash
            };
            last_lib_hash = finalized_hash;
            debug!("Send batch finalized hash: {last_lib_hash:?}",);
            self.send_batch(&batch, last_lib_hash, safe_hash).await?;
            batch.clear();
        }

        Ok(())
    }

    async fn send_batch(
        &self,
        batch: &[TelosEVMBlock],
        finalized_hash: Option<B256>,
        safe_hash: B256,
    ) -> Result<(), Error> {
        // Telos: Write extra fields to filesystem for the executor to pick up.
        // Use atomic write (tmp + rename) so reth never sees a 0-byte or partial file.
        let extra_dir = std::path::Path::new("/tmp/telos-extra-fields");
        let _ = std::fs::create_dir_all(extra_dir);
        for block in batch {
            let path = extra_dir.join(format!("{:?}.json", block.block_hash));
            let tmp_path = extra_dir.join(format!("{:?}.json.tmp", block.block_hash));
            match serde_json::to_string(&block.extra_fields) {
                Ok(json) => {
                    if json.is_empty() {
                        warn!("extra_fields serialized to empty string for block {:?}", block.block_hash);
                        continue;
                    }
                    if let Err(e) = std::fs::write(&tmp_path, &json) {
                        warn!("extra_fields tmp write failed for {:?}: {}", block.block_hash, e);
                        let _ = std::fs::remove_file(&tmp_path);
                        continue;
                    }
                    if let Err(e) = std::fs::rename(&tmp_path, &path) {
                        warn!("extra_fields rename failed for {:?}: {}", block.block_hash, e);
                        let _ = std::fs::remove_file(&tmp_path);
                    }
                }
                Err(e) => {
                    warn!("extra_fields serialize failed for {:?}: {}", block.block_hash, e);
                }
            }
        }

        let rpc_batch = batch
            .iter()
            .map(|block| {
                // TODO additional rpc call fields should be added.
                RpcRequest {
                    method: crate::execution_api_client::ExecutionApiMethod::NewPayloadV1,
                    params: vec![
                        json![block.execution_payload.clone()],
                        json![block.extra_fields.clone()],
                    ]
                    .into(),
                }
            })
            .collect::<Vec<RpcRequest>>();

        let new_payloadv1_result = self
            .execution_api
            .rpc_batch(rpc_batch)
            .await
            .map_err(|e| Error::NewPayloadV1(e.to_string()))?;

        let error_response: Vec<String> = new_payloadv1_result
            .clone()
            .into_iter()
            .filter_map(|response| response.error.map(|err| err.message))
            .collect();

        if !error_response.is_empty() {
            debug!(
                "Error sending NewPayloadV1.Result: {:?}",
                new_payloadv1_result
            );
            return Err(Error::NewPayloadV1(error_response.join("\n")));
        }

        debug!("NewPayloadV1 result: {:?}", new_payloadv1_result);

        let last_block_sent = batch.last().unwrap();

        if let Some(finalized_hash_value) = finalized_hash {
            let fork_choice_updated_result = self
                .fork_choice_updated(last_block_sent.block_hash, safe_hash, finalized_hash_value)
                .await;

            let fork_choice_updated = fork_choice_updated_result.map_err(|e| {
                debug!("Fork choice update error: {}", e);
                ForkChoiceUpdated(e.to_string())
            })?;

            if let Some(error) = fork_choice_updated.error {
                debug!("Fork choice error: {:?}", error);
                return Err(ForkChoiceUpdated(error.message));
            }

            let fork_choice_updated: ForkchoiceUpdated =
                serde_json::from_value(fork_choice_updated.result).unwrap();
            info!("fork_choice_updated_result {:?}", fork_choice_updated);

            // Valid, Invalid, Accepted, Syncing
            if fork_choice_updated.is_syncing() {
                // SYNCING is normal during initial sync - reth is catching up, just log and continue
                info!(
                    "Fork choice update status is SYNCING (reth still syncing, continuing...)",
                );
            } else if fork_choice_updated.is_invalid() {
                info!(
                    "Fork choice update status is {} ",
                    fork_choice_updated.payload_status.status
                );
                return Err(ForkChoiceUpdated(format!(
                    "Invalid status {}",
                    fork_choice_updated.payload_status.status
                )));
            }

            debug!(
                "Fork choice updated called with:\nhash {:?}\nparentHash {:?}\nnumber {:?}",
                last_block_sent.block_hash,
                last_block_sent.header.parent_hash,
                last_block_sent.block_num
            );
            info!(
                "fork_choice_updated_result for block number {}: {:?}",
                last_block_sent.block_num, fork_choice_updated
            );
        } else {
            info!(
                "Fork choice updated call skipped for block {}",
                last_block_sent.block_num
            );
        }

        Ok(())
    }

    async fn fork_choice_updated(
        &self,
        head_hash: B256,
        safe_hash: B256,
        finalized_hash: B256,
    ) -> Result<JsonResponseBody, ExecutionApiError> {
        let fork_choice_state = ForkchoiceState {
            head_block_hash: head_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        self.execution_api
            .rpc(RpcRequest {
                method: crate::execution_api_client::ExecutionApiMethod::ForkChoiceUpdatedV1,
                params: json![vec![fork_choice_state]],
            })
            .await
    }

    /// Rewind both the CL DB and the reth canonical head back to LIB. Called
    /// when a microfork is detected (see fork-handling block in `run`). LIB
    /// is the safest rewind target because Savanna guarantees blocks at or
    /// below LIB are irreversible.
    ///
    /// Steps:
    ///   1. Resolve the LIB block from the CL DB (parsed into a B256 hash).
    ///   2. Delete every CL DB entry with number > LIB.
    ///   3. Send `engine_forkchoiceUpdatedV1(head=LIB, safe=LIB,
    ///      finalized=LIB)` to reth. With `--engine.persistence-threshold`
    ///      tuned above the worst observed fork depth, reth's in-memory
    ///      buffer holds the rewound state and discards orphan ExecutedBlocks
    ///      cleanly.
    ///
    /// After this returns, the caller falls through to process the
    /// fork-triggering block as a normal forward step (it's now the new
    /// canonical block at `LIB.number + 1` or higher).
    async fn handle_reorg(
        &self,
        lib: &data::Block,
        last_lib_hash: Option<B256>,
    ) -> Result<(), Error> {
        let lib_number = lib.number;
        let lib_hash: B256 = lib.hash.parse().map_err(|_| {
            Error::Database(eyre::eyre!(
                "handle_reorg: stored LIB hash {} is not a valid B256",
                lib.hash
            ))
        })?;

        // Step 1: delete CL DB entries above LIB. Best-effort; failures
        // here are logged but don't abort the reorg — reth is the source
        // of truth post-fcU.
        if let Some(cl_latest) = self.db.get_latest_block()? {
            if cl_latest.number > lib_number {
                let mut deleted = 0u32;
                for n in (lib_number + 1)..=cl_latest.number {
                    if let Err(e) = self.db.delete_block(n) {
                        warn!("handle_reorg: delete_block({n}) failed: {e}");
                    } else {
                        deleted = deleted.saturating_add(1);
                    }
                }
                info!(
                    "handle_reorg: deleted {deleted} CL DB blocks above LIB ({lib_number})"
                );
            }
        }

        // Step 2: send fcU(head=LIB, safe=LIB, finalized=LIB) so reth
        // discards everything above LIB. Use last_lib_hash (the most recent
        // finalized hash we sent) if it differs — but the safest baseline is
        // the LIB block hash itself.
        let finalized = last_lib_hash.unwrap_or(lib_hash);
        let result = self
            .fork_choice_updated(lib_hash, lib_hash, finalized)
            .await
            .map_err(|e| ForkChoiceUpdated(format!("rewind fcU failed: {e}")))?;

        if let Some(error) = &result.error {
            return Err(ForkChoiceUpdated(format!(
                "rewind fcU returned error: {}",
                error.message
            )));
        }

        info!(
            "handle_reorg: rewound reth to LIB block {lib_number} hash {lib_hash:?} (fcU result {:?})",
            result.result
        );

        Ok(())
    }
}

/// Compare two hex-encoded hashes case-insensitively, ignoring an optional
/// `0x` prefix. Used by fork detection because `block.block_hash.to_string()`
/// (antelope `Checksum256`) and `format!("{:?}", b256)` (alloy `B256`) can
/// emit slightly different lowercase/uppercase or prefix conventions.
fn hashes_equal(a: &str, b: &str) -> bool {
    let strip = |s: &str| -> String {
        s.strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s)
            .to_ascii_lowercase()
    };
    strip(a) == strip(b)
}
