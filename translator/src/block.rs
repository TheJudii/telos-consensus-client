use crate::transaction::TelosEVMTransaction;
use crate::types::env::{ANTELOPE_EPOCH_MS, ANTELOPE_INTERVAL_MS, DEFAULT_GAS_LIMIT};
use crate::types::evm_types::{
    AccountRow, AccountStateRow, CreateAction, EvmContractConfigRow, OpenWalletAction,
    PrintedReceipt, RawAction, RpcReceipt, SetRevisionAction, TransferAction, WithdrawAction,
};
use crate::types::names::*;
use crate::types::ship_types::{
    ActionTrace, ContractRow, GetBlocksResultV0, SignedBlock, TableDelta, TransactionTrace,
};
use crate::types::translator_types::{ChainId, NameToAddressCache};
use alloy::primitives::{keccak256, Bloom, Bytes, FixedBytes, B256, U256};
use alloy_consensus::constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use alloy_consensus::{Header, Transaction, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::ExecutionPayloadV1;
use antelope::chain::checksum::Checksum256;
use antelope::chain::name::Name;
use antelope::serializer::Packer;
use eyre::eyre;
use futures_util::future::join_all;
use reth_primitives::ReceiptWithBloom;
use reth_telos_rpc_engine_api::structs::TelosEngineAPIExtraFields;
use reth_trie_common::root::ordered_trie_root_with_encoder;
use std::cmp::{max, Ordering};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tracing::{debug, warn};

const MINIMUM_FEE_PER_GAS: u128 = 7;

pub trait BasicTrace {
    fn action_name(&self) -> u64;
    fn action_account(&self) -> u64;
    fn receiver(&self) -> u64;
    fn console(&self) -> String;
    fn raw_console(&self) -> Vec<u8>;
    fn raw_return_value(&self) -> Vec<u8>;
    fn data(&self) -> Vec<u8>;
}

#[derive(Clone)]
pub enum WalletEvents {
    OpenWallet(usize, OpenWalletAction),
    CreateWallet(usize, CreateAction),
}

impl BasicTrace for ActionTrace {
    fn action_name(&self) -> u64 {
        match self {
            ActionTrace::V0(a) => a.act.name.n,
            ActionTrace::V1(a) => a.act.name.n,
        }
    }

    fn action_account(&self) -> u64 {
        match self {
            ActionTrace::V0(a) => a.act.account.n,
            ActionTrace::V1(a) => a.act.account.n,
        }
    }

    fn receiver(&self) -> u64 {
        match self {
            ActionTrace::V0(a) => a.receiver.n,
            ActionTrace::V1(a) => a.receiver.n,
        }
    }

    fn console(&self) -> String {
        // Use lossy UTF-8 decoding so that any invalid bytes (which can appear
        // in eosio.evm contract output alongside the ASCII RCPT JSON payload)
        // are replaced with U+FFFD instead of causing the entire console
        // string to be dropped. Silently dropping the console was the cause
        // of ~1470 blocks of divergence from production testnet — see
        // https://github.com/telosnetwork/telos-consensus-client issue
        match self {
            ActionTrace::V0(a) => String::from_utf8_lossy(&a.console).into_owned(),
            ActionTrace::V1(a) => String::from_utf8_lossy(&a.console).into_owned(),
        }
    }

    fn raw_console(&self) -> Vec<u8> {
        match self {
            ActionTrace::V0(a) => a.console.clone(),
            ActionTrace::V1(a) => a.console.clone(),
        }
    }

    fn raw_return_value(&self) -> Vec<u8> {
        match self {
            ActionTrace::V0(_) => Vec::new(),
            ActionTrace::V1(a) => a.return_value.clone(),
        }
    }

    fn data(&self) -> Vec<u8> {
        match self {
            ActionTrace::V0(a) => a.act.data.clone(),
            ActionTrace::V1(a) => a.act.data.clone(),
        }
    }
}

#[derive(Clone)]
pub enum DecodedRow {
    Config(EvmContractConfigRow),
    Account(bool, AccountRow),
    AccountState(bool, AccountStateRow, Name),
}

#[derive(Clone)]
pub struct ProcessingEVMBlock {
    pub block_num: u32,
    pub block_hash: Checksum256,
    pub prev_block_hash: Option<Checksum256>,
    chain_id: u64,
    result: GetBlocksResultV0,
    signed_block: Option<SignedBlock>,
    block_traces: Option<Vec<TransactionTrace>>,
    contract_rows: Option<Vec<(bool, ContractRow)>>,
    cumulative_gas_used: u64,
    dyn_gas_limit: Option<u128>,
    pub decoded_rows: Vec<DecodedRow>,
    pub transactions: Vec<(TelosEVMTransaction, ReceiptWithBloom)>,
    pub new_gas_price: Option<(u64, U256)>,
    pub new_revision: Option<(u64, u64)>,
    pub new_wallets: Vec<WalletEvents>,
    pub lib_num: u32,
    pub lib_hash: Checksum256,
    pub skip_events: bool,
    rpc_fallback_endpoints: Vec<String>,
    rpc_fallback_quorum: usize,
    rpc_fallback_sample_every_n: u32,
    block_timestamp: u64,
    /// Count of tx-bearing eosio.evm actions observed during handle_action.
    /// Used to gate the RPC-validation skip path: skip only when this is 0
    /// (genuine-empty evidence), not when self.transactions is empty
    /// (which can falsely mean "we dropped them silently"). See block.rs
    /// commentary around the skip path for full rationale.
    pub tx_actions_seen: u32,
    missing_receipt_tx_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TelosEVMBlock {
    pub block_num: u32,
    pub block_hash: B256,
    pub ship_hash: String,
    pub lib_num: u32,
    pub lib_hash: String,
    pub header: Header,
    pub transactions: Vec<(TelosEVMTransaction, ReceiptWithBloom)>,
    pub execution_payload: ExecutionPayloadV1,
    pub extra_fields: TelosEngineAPIExtraFields,
}

pub enum GeneratedEvmData {
    Canonical {
        header: Header,
        execution_payload: ExecutionPayloadV1,
    },
    NonCanonical {
        evm_block_num: u32,
        local_hash: B256,
        reference_hash: B256,
    },
    ValidationUnavailable {
        reason: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum HashValidation {
    Canonical,
    NonCanonical { reference_hash: B256 },
    QuorumUnavailable { reason: String },
}

enum TxMembershipValidation {
    Included { reference_hash: B256 },
    Omitted { reference_hash: B256 },
    QuorumUnavailable { reason: String },
}

impl TelosEVMBlock {
    pub fn lib_evm_num(&self, chain_id: &ChainId) -> u32 {
        self.lib_num.saturating_sub(chain_id.block_delta())
    }

    pub fn block_num_with_delta(&self, chain_id: &ChainId) -> u32 {
        self.block_num + chain_id.block_delta()
    }

    pub fn is_final(&self, chain_id: &ChainId) -> bool {
        self.block_num_with_delta(chain_id) <= self.lib_num
    }

    pub fn is_lib(&self, chain_id: &ChainId) -> bool {
        self.block_num_with_delta(chain_id) == self.lib_num
    }
}

pub fn decode_raw_action(encoded: &[u8]) -> RawAction {
    decode::<RawAction>(encoded)
}

pub fn decode<T: Packer + Default>(raw: &[u8]) -> T {
    let mut result = T::default();
    result.unpack(raw);
    result
}

pub struct ProcessingEVMBlockArgs {
    pub chain_id: u64,
    pub block_num: u32,
    pub block_hash: Checksum256,
    pub prev_block_hash: Option<Checksum256>,
    pub lib_num: u32,
    pub lib_hash: Checksum256,
    pub result: GetBlocksResultV0,
    pub skip_events: bool,
    pub rpc_fallback_endpoints: Vec<String>,
    pub rpc_fallback_quorum: usize,
    pub rpc_fallback_sample_every_n: u32,
    pub block_timestamp: u64,
}

impl ProcessingEVMBlock {
    pub fn new(args: ProcessingEVMBlockArgs) -> Self {
        let ProcessingEVMBlockArgs {
            chain_id,
            block_num,
            block_hash,
            prev_block_hash,
            lib_num,
            lib_hash,
            result,
            skip_events,
            rpc_fallback_endpoints,
            rpc_fallback_quorum,
            rpc_fallback_sample_every_n,
            block_timestamp,
        } = args;

        Self {
            block_num,
            block_hash,
            prev_block_hash,
            lib_num,
            lib_hash,
            chain_id,
            result,
            skip_events,
            signed_block: None,
            block_traces: None,
            contract_rows: None,
            cumulative_gas_used: 0,
            dyn_gas_limit: None,
            decoded_rows: vec![],
            transactions: vec![],

            new_gas_price: None,
            new_revision: None,
            new_wallets: vec![],
            rpc_fallback_endpoints,
            rpc_fallback_quorum,
            rpc_fallback_sample_every_n,
            block_timestamp,
            tx_actions_seen: 0,
            missing_receipt_tx_hash: None,
        }
    }

    pub fn deserialize(&mut self) {
        self.signed_block = self.result.block.as_deref().map(decode);

        if self.result.traces.is_none() {
            warn!("No block traces found for block: {}", self.block_num);
        }

        self.block_traces = self.result.traces.as_deref().map(decode).or(Some(vec![]));

        if self.result.deltas.is_none() {
            warn!("No deltas found for block: {}", self.block_num);
        };

        // TODO: Handle present: false here?  How to account for empty/deleted rows?
        self.contract_rows = self.result.deltas.as_deref().map(|deltas| {
            decode::<Vec<TableDelta>>(deltas)
                .iter()
                .filter_map(|TableDelta::V0(delta)| {
                    if delta.name == "contract_row" {
                        Some(
                            delta
                                .rows
                                .iter()
                                .map(|row| {
                                    let contract_row = decode::<ContractRow>(row.data.as_slice());
                                    (row.present, contract_row) // row.present becomes the first item in the tuple
                                })
                                .collect::<Vec<(bool, ContractRow)>>(),
                        )
                    } else {
                        None
                    }
                })
                .flatten()
                .collect::<Vec<(bool, ContractRow)>>()
        });
    }

    fn find_config_row(&self) -> Option<&EvmContractConfigRow> {
        self.decoded_rows.iter().find_map(|row| {
            if let DecodedRow::Config(config) = row {
                Some(config)
            } else {
                None
            }
        })
    }

    fn add_transaction(&mut self, transaction: TelosEVMTransaction) {
        let full_receipt = transaction.receipt(self.cumulative_gas_used);
        let gas_limit = transaction.envelope.gas_limit() + self.cumulative_gas_used as u128;
        self.cumulative_gas_used = full_receipt.receipt.cumulative_gas_used;
        self.transactions.push((transaction, full_receipt));

        if self.dyn_gas_limit.is_none() {
            self.dyn_gas_limit = Some(gas_limit);
        } else if gas_limit > self.dyn_gas_limit.unwrap() {
            self.dyn_gas_limit = Some(gas_limit)
        }
    }

    fn effective_rpc_quorum(&self) -> usize {
        let endpoint_count = self.rpc_fallback_endpoints.len();
        if endpoint_count == 0 {
            0
        } else {
            self.rpc_fallback_quorum.clamp(1, endpoint_count)
        }
    }

    fn has_canonical_rpc(&self) -> bool {
        !self.rpc_fallback_endpoints.is_empty() && self.effective_rpc_quorum() > 0
    }

    async fn raw_action_transaction_hash(&self, raw: &RawAction) -> eyre::Result<String> {
        let transaction = TelosEVMTransaction::from_raw_action(
            self.chain_id,
            self.transactions.len(),
            self.block_hash,
            raw.clone(),
            PrintedReceipt::default(),
        )
        .await
        .map_err(|e| eyre!("failed to build raw action transaction hash: {e}"))?;

        Ok(format!("0x{}", hex::encode(transaction.hash().as_slice())))
    }

    async fn fetch_receipt_from_rpc(&self, tx_hash: &str) -> eyre::Result<Option<RpcReceipt>> {
        if self.rpc_fallback_endpoints.is_empty() {
            return Err(eyre!("No RPC fallback endpoints configured"));
        }

        let mut null_endpoints = Vec::new();
        let mut errors = Vec::new();

        for rpc_endpoint in &self.rpc_fallback_endpoints {
            match Self::fetch_receipt_from_rpc_endpoint(rpc_endpoint, tx_hash).await {
                Ok(Some(receipt)) => return Ok(Some(receipt)),
                Ok(None) => null_endpoints.push(rpc_endpoint.clone()),
                Err(error) => errors.push(format!("{rpc_endpoint}: {error}")),
            }
        }

        if !null_endpoints.is_empty() {
            return Ok(None);
        }

        Err(eyre!(
            "all RPC receipt fallbacks failed for tx {}: {}",
            tx_hash,
            errors.join("; ")
        ))
    }

    async fn fetch_receipt_from_rpc_endpoint(
        rpc_endpoint: &str,
        tx_hash: &str,
    ) -> eyre::Result<Option<RpcReceipt>> {
        // Build JSON-RPC request for eth_getTransactionReceipt
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
            "id": 1,
        });

        use std::sync::LazyLock;
        static RPC_CLIENT_RECEIPT: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(4)
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default()
        });

        let response = RPC_CLIENT_RECEIPT
            .post(rpc_endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| eyre!("Failed to fetch receipt from RPC: {}", e))?;

        let json_response: serde_json::Value = response
            .json()
            .await
            .map_err(|e| eyre!("Failed to parse RPC response: {}", e))?;

        if let Some(error) = json_response.get("error") {
            return Err(eyre!("RPC error: {}", error));
        }

        let receipt_data = json_response
            .get("result")
            .ok_or_else(|| eyre!("No result in RPC response"))?;

        // If the RPC returns null, the transaction doesn't exist on the
        // reference chain — the caller should skip it.
        if receipt_data.is_null() {
            return Ok(None);
        }

        let receipt: RpcReceipt = serde_json::from_value(receipt_data.clone())
            .map_err(|e| eyre!("Failed to parse receipt from RPC: {}", e))?;

        Ok(Some(receipt))
    }

    async fn fetch_block_hash_from_rpc(
        rpc_endpoint: String,
        evm_block_num: u64,
    ) -> (String, Result<B256, String>) {
        let block_hex = format!("0x{:x}", evm_block_num);
        let hash_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": [block_hex, false],
            "id": 1,
        });

        use std::sync::LazyLock;
        static RPC_CLIENT_HASH: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(4)
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default()
        });

        let result = async {
            let response = RPC_CLIENT_HASH
                .post(&rpc_endpoint)
                .json(&hash_request)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;

            if !response.status().is_success() {
                return Err(format!("HTTP {}", response.status()));
            }

            let json_response = response
                .json::<serde_json::Value>()
                .await
                .map_err(|e| format!("invalid JSON response: {e}"))?;

            if let Some(error) = json_response.get("error") {
                return Err(format!("RPC error: {error}"));
            }

            let result = json_response
                .get("result")
                .ok_or_else(|| "missing result".to_string())?;
            if result.is_null() {
                return Err("block unavailable".to_string());
            }

            let hash = result["hash"]
                .as_str()
                .ok_or_else(|| "missing hash".to_string())?
                .parse::<B256>()
                .map_err(|e| format!("invalid hash: {e}"))?;

            Ok(hash)
        }
        .await;

        (rpc_endpoint, result)
    }

    async fn validate_hash_with_quorum(
        &self,
        evm_block_num: u64,
        our_hash: B256,
    ) -> HashValidation {
        let quorum = self.effective_rpc_quorum();
        if quorum == 0 {
            return HashValidation::Canonical;
        }

        let results = join_all(
            self.rpc_fallback_endpoints
                .iter()
                .cloned()
                .map(|endpoint| Self::fetch_block_hash_from_rpc(endpoint, evm_block_num)),
        )
        .await;

        let mut votes: HashMap<B256, usize> = HashMap::new();
        let mut errors = Vec::new();
        let mut reachable = 0usize;

        for (endpoint, result) in results {
            match result {
                Ok(hash) => {
                    reachable += 1;
                    *votes.entry(hash).or_default() += 1;
                }
                Err(error) => errors.push(format!("{endpoint}: {error}")),
            }
        }

        Self::classify_hash_votes(
            evm_block_num,
            our_hash,
            quorum,
            self.rpc_fallback_endpoints.len(),
            &votes,
            reachable,
            &errors,
        )
    }

    fn classify_hash_votes(
        evm_block_num: u64,
        our_hash: B256,
        quorum: usize,
        endpoint_count: usize,
        votes: &HashMap<B256, usize>,
        reachable: usize,
        errors: &[String],
    ) -> HashValidation {
        let mut tallies = votes
            .iter()
            .map(|(hash, votes)| format!("{hash}={votes}"))
            .collect::<Vec<_>>();
        tallies.sort();

        let quorum_unavailable = |detail: String| {
            HashValidation::QuorumUnavailable {
                reason: format!(
                    "canonical hash quorum unavailable for EVM block {}: {}; reachable={}/{}, quorum={}, tallies=[{}], errors=[{}]",
                    evm_block_num,
                    detail,
                    reachable,
                    endpoint_count,
                    quorum,
                    tallies.join(", "),
                    errors.join("; ")
                ),
            }
        };

        if quorum == 1 && votes.len() > 1 {
            return quorum_unavailable(
                "reference RPCs disagree while quorum=1; refusing to choose a canonical hash from a single vote".to_string(),
            );
        }

        if votes.get(&our_hash).copied().unwrap_or_default() >= quorum {
            return HashValidation::Canonical;
        }

        if let Some((reference_hash, votes)) = votes.iter().max_by_key(|(_, votes)| *votes) {
            if *votes >= quorum {
                if *votes < 2 {
                    return quorum_unavailable(format!(
                        "refusing to mark local SHIP hash noncanonical with only {votes} reference RPC vote(s)"
                    ));
                }

                return HashValidation::NonCanonical {
                    reference_hash: *reference_hash,
                };
            }
        }

        HashValidation::QuorumUnavailable {
            reason: format!(
                "canonical hash quorum unavailable for EVM block {}: reachable={}/{}, quorum={}, tallies=[{}], errors=[{}]",
                evm_block_num,
                reachable,
                endpoint_count,
                quorum,
                tallies.join(", "),
                errors.join("; ")
            ),
        }
    }

    async fn fetch_block_tx_membership_from_rpc(
        rpc_endpoint: String,
        evm_block_num: u64,
        tx_hash: String,
    ) -> (String, Result<(B256, bool), String>) {
        let block_hex = format!("0x{:x}", evm_block_num);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": [block_hex, false],
            "id": 1,
        });

        use std::sync::LazyLock;
        static RPC_CLIENT_TX_MEMBERSHIP: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(4)
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default()
        });

        let result = async {
            let response = RPC_CLIENT_TX_MEMBERSHIP
                .post(&rpc_endpoint)
                .json(&request)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;

            if !response.status().is_success() {
                return Err(format!("HTTP {}", response.status()));
            }

            let json_response = response
                .json::<serde_json::Value>()
                .await
                .map_err(|e| format!("invalid JSON response: {e}"))?;

            if let Some(error) = json_response.get("error") {
                return Err(format!("RPC error: {error}"));
            }

            let result = json_response
                .get("result")
                .ok_or_else(|| "missing result".to_string())?;
            if result.is_null() {
                return Err("block unavailable".to_string());
            }

            let block_hash = result["hash"]
                .as_str()
                .ok_or_else(|| "missing hash".to_string())?
                .parse::<B256>()
                .map_err(|e| format!("invalid hash: {e}"))?;

            let contains_tx = result["transactions"]
                .as_array()
                .map(|transactions| {
                    transactions.iter().any(|tx| {
                        tx.as_str()
                            .or_else(|| tx.get("hash").and_then(|hash| hash.as_str()))
                            .is_some_and(|hash| hash.eq_ignore_ascii_case(&tx_hash))
                    })
                })
                .unwrap_or(false);

            Ok((block_hash, contains_tx))
        }
        .await;

        (rpc_endpoint, result)
    }

    async fn validate_tx_membership_with_quorum(
        &self,
        evm_block_num: u64,
        tx_hash: &str,
    ) -> TxMembershipValidation {
        let quorum = self.effective_rpc_quorum();
        if quorum == 0 {
            return TxMembershipValidation::QuorumUnavailable {
                reason: "no canonical RPC quorum configured".to_string(),
            };
        }

        let results = join_all(self.rpc_fallback_endpoints.iter().cloned().map(|endpoint| {
            Self::fetch_block_tx_membership_from_rpc(endpoint, evm_block_num, tx_hash.to_string())
        }))
        .await;

        let mut by_hash: HashMap<B256, Vec<bool>> = HashMap::new();
        let mut errors = Vec::new();
        let mut reachable = 0usize;

        for (endpoint, result) in results {
            match result {
                Ok((block_hash, contains_tx)) => {
                    reachable += 1;
                    by_hash.entry(block_hash).or_default().push(contains_tx);
                }
                Err(error) => errors.push(format!("{endpoint}: {error}")),
            }
        }

        if let Some((reference_hash, memberships)) = by_hash
            .iter()
            .max_by_key(|(_, memberships)| memberships.len())
        {
            if memberships.len() >= quorum {
                if memberships.iter().any(|contains_tx| *contains_tx) {
                    return TxMembershipValidation::Included {
                        reference_hash: *reference_hash,
                    };
                }

                return TxMembershipValidation::Omitted {
                    reference_hash: *reference_hash,
                };
            }
        }

        let mut tallies = by_hash
            .iter()
            .map(|(hash, memberships)| {
                let includes = memberships
                    .iter()
                    .filter(|contains_tx| **contains_tx)
                    .count();
                format!(
                    "{hash}=votes:{},tx_included:{}",
                    memberships.len(),
                    includes
                )
            })
            .collect::<Vec<_>>();
        tallies.sort();

        TxMembershipValidation::QuorumUnavailable {
            reason: format!(
                "canonical tx membership quorum unavailable for EVM block {}, tx {}: reachable={}/{}, quorum={}, tallies=[{}], errors=[{}]",
                evm_block_num,
                tx_hash,
                reachable,
                self.rpc_fallback_endpoints.len(),
                quorum,
                tallies.join(", "),
                errors.join("; ")
            ),
        }
    }

    async fn handle_action(
        &mut self,
        action: Box<dyn BasicTrace + Send>,
        native_to_evm_cache: &NameToAddressCache,
        evm_block_num: u64,
    ) -> eyre::Result<()> {
        let action_name = action.action_name();
        let action_account = action.action_account();
        let action_receiver = action.receiver();

        // Evidence counter for tx-bearing actions. Increment BEFORE any parse
        // step so that a silent parse failure downstream doesn't also silently
        // zero-out our evidence. Used to gate RPC-validation skip below.
        if action_account == EOSIO_EVM
            && (action_name == RAW || action_name == WITHDRAW || action_name == TRANSFER)
        {
            self.tx_actions_seen = self.tx_actions_seen.saturating_add(1);
        }

        if action_account == EOSIO_EVM && action_name == INIT {
            let config_delta_row = self
                .find_config_row()
                .expect("Table delta for the init action not found");

            let gas_price = U256::from_be_slice(&config_delta_row.gas_price.data);

            self.new_gas_price = Some((self.transactions.len() as u64, gas_price));
        } else if action_account == EOSIO_EVM && action_name == RAW {
            // Normally signed EVM transaction
            let raw: RawAction = decode_raw_action(&action.data());
            // PrintedReceipt is parsed from the eosio.evm action console output
            // (the RCPT{{...}}RCPT JSON payload). Starting from Telos testnet
            // block 414859739 (Dec 2024 hardfork / eosio.evm contract upgrade)
            // the contract stopped emitting the RCPT payload to console for
            // regular raw actions. When this happens we MUST NOT silently drop
            // the transaction — doing so causes the block to be sent to reth
            // with an empty transactions list, diverging permanently from
            // production.
            //
            // Fallback mechanism: if no PrintedReceipt is found in console output
            // and RPC fallback endpoints are configured, fetch the receipt from the
            // RPC endpoint using eth_getTransactionReceipt.
            let raw_console_bytes = action.raw_console();
            let printed_receipt = match PrintedReceipt::from_console(action.console()) {
                Some(r) => r,
                None => {
                    // Try RPC fallback if configured
                    if self.has_canonical_rpc() {
                        let raw_payload_hash = keccak256(&raw.tx);
                        let raw_payload_hash_str = format!("0x{}", hex::encode(raw_payload_hash));
                        let tx_hash_str = self.raw_action_transaction_hash(&raw).await?;
                        if tx_hash_str != raw_payload_hash_str {
                            debug!(
                                "Raw action payload hash {} translated to EVM transaction hash {} in block {}",
                                raw_payload_hash_str, tx_hash_str, self.block_num
                            );
                        }

                        match self.fetch_receipt_from_rpc(&tx_hash_str).await {
                            Ok(Some(rpc_receipt)) => {
                                debug!(
                                    "Fetched receipt from RPC fallback for tx {} in block {}",
                                    tx_hash_str, self.block_num
                                );
                                PrintedReceipt::from_rpc_receipt(
                                    &rpc_receipt,
                                    self.transactions.len() as u16,
                                    self.block_num as u64,
                                    self.block_timestamp,
                                )
                            }
                            Ok(None) => {
                                warn!(
                                    "Single-tx receipt RPC returned null for tx {} in block {}. Checking canonical tx membership before retrying.",
                                    tx_hash_str, self.block_num
                                );

                                match self
                                    .validate_tx_membership_with_quorum(evm_block_num, &tx_hash_str)
                                    .await
                                {
                                    TxMembershipValidation::Included { reference_hash } => {
                                        self.missing_receipt_tx_hash = Some(tx_hash_str.clone());
                                        return Err(eyre::eyre!(
                                            "receipt unavailable for tx {} in native block {} (EVM {}), but canonical RPC quorum includes it in block {}; waiting for receipt availability",
                                            tx_hash_str,
                                            self.block_num,
                                            evm_block_num,
                                            reference_hash
                                        ));
                                    }
                                    TxMembershipValidation::Omitted { reference_hash } => {
                                        warn!(
                                            "Canonical RPC quorum omits tx {} from native block {} (EVM {}, reference block {}). Skipping this raw action and forcing block-hash validation.",
                                            tx_hash_str,
                                            self.block_num,
                                            evm_block_num,
                                            reference_hash
                                        );
                                        return Ok(());
                                    }
                                    TxMembershipValidation::QuorumUnavailable { reason } => {
                                        self.missing_receipt_tx_hash = Some(tx_hash_str.clone());
                                        return Err(eyre::eyre!(
                                            "receipt unavailable for tx {} in native block {} (EVM {}), and canonical tx membership could not be verified: {}",
                                            tx_hash_str,
                                            self.block_num,
                                            evm_block_num,
                                            reason
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "RPC fallback failed for tx {} in block {}: {}",
                                    tx_hash_str, self.block_num, e
                                );
                                let hex_prefix: String = raw_console_bytes
                                    .iter()
                                    .take(256)
                                    .map(|b| format!("{:02x}", b))
                                    .collect();
                                return Err(eyre::eyre!(
                                    "No parseable PrintedReceipt and RPC fallback failed \
                                     for raw action in block {}: raw console bytes len={}, \
                                     first 256 bytes hex={:?}, RPC error: {}",
                                    self.block_num,
                                    raw_console_bytes.len(),
                                    hex_prefix,
                                    e,
                                ));
                            }
                        }
                    } else {
                        let hex_prefix: String = raw_console_bytes
                            .iter()
                            .take(256)
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        return Err(eyre::eyre!(
                            "No parseable PrintedReceipt for raw action in block {}: \
                             raw console bytes len={}, first 256 bytes hex={:?}. \
                             Configure rpc_fallback_endpoints to use RPC fallback.",
                            self.block_num,
                            raw_console_bytes.len(),
                            hex_prefix,
                        ));
                    }
                }
            };

            let transaction = TelosEVMTransaction::from_raw_action(
                self.chain_id,
                self.transactions.len(),
                self.block_hash,
                raw,
                printed_receipt,
            )
            .await?;

            self.add_transaction(transaction);
            return Ok(());
        } else if action_account == EOSIO_EVM && action_name == WITHDRAW {
            // Withdrawal from EVM
            let withdraw_action: WithdrawAction = decode(&action.data());
            let transaction = TelosEVMTransaction::from_withdraw(
                self.chain_id,
                self.transactions.len(),
                self.block_hash,
                withdraw_action,
                native_to_evm_cache,
            )
            .await?;
            self.add_transaction(transaction);
        } else if action_account == EOSIO_TOKEN
            && action_name == TRANSFER
            && action_receiver == EOSIO_TOKEN
        {
            // Deposit/transfer to EVM
            let transfer_action: TransferAction = decode(&action.data());
            if transfer_action.to.n != EOSIO_EVM
                || SYSTEM_ACCOUNTS.contains(&transfer_action.from.n)
            {
                return Ok(());
            }

            let transaction = TelosEVMTransaction::from_transfer(
                self.chain_id,
                self.transactions.len(),
                self.block_hash,
                transfer_action,
                native_to_evm_cache,
            )
            .await?;
            self.add_transaction(transaction);
        } else if action_account == EOSIO_EVM && action_name == DORESOURCES {
            let config_delta_row = self
                .find_config_row()
                .expect("Table delta for the doresources action not found");

            let gas_price = U256::from_be_slice(&config_delta_row.gas_price.data);

            self.new_gas_price = Some((self.transactions.len() as u64, gas_price));
        } else if action_account == EOSIO_EVM && action_name == SETREVISION {
            let rev_action: SetRevisionAction = decode(&action.data());

            self.new_revision = Some((
                self.transactions.len() as u64,
                rev_action.new_revision as u64,
            ));
        } else if action_account == EOSIO_EVM && action_name == OPENWALLET {
            let wallet_action: OpenWalletAction = decode(&action.data());

            self.new_wallets.push(WalletEvents::OpenWallet(
                self.transactions.len(),
                wallet_action,
            ));
        } else if action_account == EOSIO_EVM && action_name == CREATE {
            let wallet_action: CreateAction = decode(&action.data());
            self.new_wallets.push(WalletEvents::CreateWallet(
                self.transactions.len(),
                wallet_action,
            ));
        }
        Ok(())
    }

    pub async fn generate_evm_data(
        &mut self,
        parent_hash: FixedBytes<32>,
        block_delta: u32,
        native_to_evm_cache: &NameToAddressCache,
    ) -> eyre::Result<GeneratedEvmData> {
        if self.signed_block.is_none()
            || self.block_traces.is_none()
            || self.contract_rows.is_none()
        {
            panic!("Block::to_evm called on a block with missing data");
        }

        let row_deltas = self.contract_rows.clone().unwrap_or_default();

        let mut deduped_accstate_deltas = BTreeMap::new();

        if !self.skip_events {
            for delta in row_deltas {
                match delta.1 {
                    ContractRow::V0(r) => {
                        // Global eosio.system table, since block_delta is static
                        // no need to decode
                        // if r.table == Name::new_from_str("global") {
                        //     let mut decoder = Decoder::new(r.value.as_slice());
                        //     let decoded_row = &mut GlobalTable::default();
                        //     decoder.unpack(decoded_row);
                        //     info!("Global table: {:?}", decoded_row);
                        // }
                        if r.code == Name::new_from_str("eosio.evm") {
                            // delta.0 is "present" and if false, the row was removed
                            let removed = !delta.0;
                            if r.table == Name::new_from_str("config") {
                                if removed {
                                    panic!(
                                        "Config row removed, this should never happen: {}",
                                        self.block_num
                                    );
                                }
                                self.decoded_rows.push(DecodedRow::Config(decode(&r.value)));
                            } else if r.table == Name::new_from_str("account") {
                                self.decoded_rows
                                    .push(DecodedRow::Account(removed, decode(&r.value)));
                            } else if r.table == Name::new_from_str("accountstate") {
                                let decoded_row: AccountStateRow = decode(&r.value);
                                let complex_key = (r.scope.n, decoded_row.key.data);
                                match deduped_accstate_deltas.get(&complex_key) {
                                    Some(prev_acc_state) => match prev_acc_state {
                                        DecodedRow::AccountState(_vrem, prev_row, _scope) => {
                                            if prev_row.index < decoded_row.index {
                                                deduped_accstate_deltas.insert(
                                                    complex_key,
                                                    DecodedRow::AccountState(
                                                        removed,
                                                        decoded_row,
                                                        r.scope,
                                                    ),
                                                );
                                            }
                                        }
                                        _ => {
                                            panic!("Not suposed to happen");
                                        }
                                    },
                                    None => {
                                        deduped_accstate_deltas.insert(
                                            complex_key,
                                            DecodedRow::AccountState(removed, decoded_row, r.scope),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for row in deduped_accstate_deltas.values() {
                self.decoded_rows.push(row.clone());
            }

            let traces = self.block_traces.clone().unwrap_or_default();
            let mut action_processing_error = None;
            let evm_block_num = (self.block_num - block_delta) as u64;

            for TransactionTrace::V0(t) in traces {
                for action in t.action_traces {
                    if let Err(e) = self
                        .handle_action(Box::new(action), native_to_evm_cache, evm_block_num)
                        .await
                    {
                        warn!(
                            "Error handling action in block {} (EVM {}): {}. Will stall and retry.",
                            self.block_num, evm_block_num, e
                        );
                        action_processing_error = Some(e.to_string());
                        break;
                    }
                }
                if action_processing_error.is_some() {
                    break;
                }
            }

            // Do not synthesize a block from eth_getBlockByNumber after an
            // action parse failure. RPC block payloads do not carry Telos
            // extra-fields (state diffs, wallet mappings, receipt vector) in
            // the format reth needs. Stalling and retrying is safer than
            // committing a payload whose execution sidecar is incomplete.
            if let Some(error) = action_processing_error {
                let evm_block_num = (self.block_num - block_delta) as u64;
                if let Some(tx_hash) = self.missing_receipt_tx_hash.clone() {
                    match self
                        .validate_tx_membership_with_quorum(evm_block_num, &tx_hash)
                        .await
                    {
                        TxMembershipValidation::Omitted { reference_hash } => {
                            return Ok(GeneratedEvmData::ValidationUnavailable {
                                reason: format!(
                                    "action processing failed for tx {} in native block {} (EVM {}), and canonical RPC quorum omits it from reference block {}; refusing to synthesize a fork marker after partial processing",
                                    tx_hash, self.block_num, evm_block_num, reference_hash
                                ),
                            });
                        }
                        TxMembershipValidation::Included { reference_hash } => {
                            return Ok(GeneratedEvmData::ValidationUnavailable {
                                reason: format!(
                                    "receipt unavailable for tx {} in native block {} (EVM {}), but canonical RPC quorum includes it in block {}; waiting for receipt availability",
                                    tx_hash, self.block_num, evm_block_num, reference_hash
                                ),
                            });
                        }
                        TxMembershipValidation::QuorumUnavailable { reason } => {
                            return Ok(GeneratedEvmData::ValidationUnavailable {
                                reason: format!(
                                    "receipt unavailable for tx {} in native block {} (EVM {}), and canonical tx membership could not be verified: {}",
                                    tx_hash, self.block_num, evm_block_num, reason
                                ),
                            });
                        }
                    }
                }

                return Ok(GeneratedEvmData::ValidationUnavailable {
                    reason: format!(
                        "action processing failed for native block {} (EVM {}): {}",
                        self.block_num, evm_block_num, error
                    ),
                });
            }

            // This is an exception for the wrong deployment of EVM contract in the testnet on native block #276210867 which caused revision become zero
            if self.chain_id == 41 && self.block_num == 276210867 {
                self.new_revision = Some((0, 0));
            }
        }

        let tx_root_hash =
            ordered_trie_root_with_encoder(&self.transactions, |(tx, _receipt), buf| {
                match &tx.envelope {
                    TxEnvelope::Legacy(_stx) => tx.envelope.encode(buf),
                    envelope => {
                        buf.push(u8::from(envelope.tx_type()));

                        if envelope.is_eip1559() {
                            let stx = envelope.as_eip1559().unwrap();
                            stx.tx().encode_with_signature_fields(stx.signature(), buf);
                        } else {
                            panic!("unimplemented tx type");
                        }
                    }
                }
            });
        let receipts_root_hash =
            ordered_trie_root_with_encoder(&self.transactions, |(_trx, r), buf| r.encode(buf));
        let mut logs_bloom = Bloom::default();
        for (_trx, receipt) in &self.transactions {
            logs_bloom.accrue_bloom(&receipt.bloom);
        }

        let gas_limit = if let Some(dyn_gas) = self.dyn_gas_limit {
            debug!("Dynamic gas limit: {}", dyn_gas);
            max(DEFAULT_GAS_LIMIT, dyn_gas)
        } else {
            DEFAULT_GAS_LIMIT
        };

        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Default::default(),
            state_root: EMPTY_ROOT_HASH,
            transactions_root: tx_root_hash,
            receipts_root: receipts_root_hash,
            withdrawals_root: None,
            logs_bloom,
            difficulty: Default::default(),
            number: (self.block_num - block_delta) as u64,
            gas_limit,
            gas_used: self.cumulative_gas_used as u128,
            timestamp: (((self.signed_block.clone().unwrap().header.header.timestamp as u64)
                * ANTELOPE_INTERVAL_MS)
                + ANTELOPE_EPOCH_MS)
                / 1000,
            mix_hash: Default::default(),
            nonce: Default::default(),
            base_fee_per_gas: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_root: None,
            extra_data: Bytes::from(self.block_hash.data),
        };

        let base_fee_per_gas = U256::from(
            header
                .base_fee_per_gas
                .filter(|&fee| fee > MINIMUM_FEE_PER_GAS)
                .unwrap_or(MINIMUM_FEE_PER_GAS),
        );

        let transactions = self
            .transactions
            .iter()
            .map(|(transaction, _receipt)| {
                let mut encoded = vec![];
                transaction.envelope.encode_2718(&mut encoded);
                Bytes::from(encoded)
            })
            .collect::<Vec<_>>();

        let exec_payload = ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Default::default(),
            state_root: EMPTY_ROOT_HASH,
            receipts_root: receipts_root_hash,
            logs_bloom,
            prev_randao: B256::ZERO,
            block_number: header.number,
            gas_limit: header.gas_limit as u64,
            gas_used: header.gas_used as u64,
            timestamp: header.timestamp,
            extra_data: header.extra_data.clone(),
            base_fee_per_gas,
            block_hash: header.hash_slow(),
            transactions,
        };

        // Block-level hash verification against reference RPC.
        // In pre-Savannah head-tracking mode, a mismatch means local SHIP is
        // showing a reversible fork block. Do not synthesize a canonical block
        // from RPC here: RPC does not provide the CL extra-fields needed by
        // reth's build_state path. Instead, report the noncanonical block and
        // let the final processor skip it until SHIP provides the canonical fork.
        //
        // Sampling remains config-driven for finalized-only tooling, but
        // pre-Savannah head-tracking overrides the sample rate to 1 before
        // blocks reach this point.
        if self.has_canonical_rpc() {
            let our_hash = exec_payload.block_hash;
            let evm_block_num = header.number;
            // Apr 2026 re-enable: the Path 1 disable was based on a wrong
            // hypothesis (translator producing bad empty-block hashes).
            // Instrumentation proved the translator + RPC fallback were
            // correct; the crash loop was a reth CanonicalHeaders index
            // inconsistency at one block, handled by consensus client's
            // check_range by-hash fallthrough.
            // Sampling rate is config-driven. n=1 means every block is validated
            // (safe production default); n=10 means 1-in-10 empty blocks are sampled
            // (quick-sync profile). Blocks with transactions are ALWAYS validated
            // regardless of sampling, because tx-level fallbacks depend on it.
            let sample_every_n = self.rpc_fallback_sample_every_n.max(1) as u64;
            // Evidence-based skip: only consider the block "genuinely empty"
            // (and therefore skip-validation-eligible) if we positively observed
            // zero tx-bearing actions during parsing. self.transactions.is_empty()
            // alone is NOT sufficient evidence — it's also what we'd see if
            // parsing silently dropped txs. See mainnet-quick incident:
            // block 463,899,714 had 1 canonical tx, translator silently dropped
            // it on null RPC receipt, output was empty, old logic skipped
            // validation, reth committed the wrong hash permanently.
            let had_no_tx_actions = self.tx_actions_seen == 0;
            let has_transactions = !self.transactions.is_empty();
            let is_sample_block = sample_every_n <= 1 || evm_block_num % sample_every_n == 0;
            // Defensive cross-check: if we saw tx actions but produced no output
            // txs, that's a silent drop. Log loudly before forcing validation.
            if !has_transactions && !had_no_tx_actions {
                warn!(
                    "Block {}: observed {} tx-bearing actions but produced 0 output txs. Forcing RPC validation (silent-drop suspected).",
                    evm_block_num, self.tx_actions_seen
                );
            }
            if had_no_tx_actions && !is_sample_block {
                return Ok(GeneratedEvmData::Canonical {
                    header,
                    execution_payload: exec_payload,
                });
            }

            match self
                .validate_hash_with_quorum(evm_block_num, our_hash)
                .await
            {
                HashValidation::Canonical => {}
                HashValidation::NonCanonical { reference_hash } => {
                    warn!(
                        "Block {} is not canonical: local SHIP hash={} reference={}. Skipping until SHIP provides the canonical fork.",
                        evm_block_num, our_hash, reference_hash
                    );
                    return Ok(GeneratedEvmData::NonCanonical {
                        evm_block_num: evm_block_num as u32,
                        local_hash: our_hash,
                        reference_hash,
                    });
                }
                HashValidation::QuorumUnavailable { reason } => {
                    warn!("{reason}");
                    return Ok(GeneratedEvmData::ValidationUnavailable { reason });
                }
            }
        }

        Ok(GeneratedEvmData::Canonical {
            header,
            execution_payload: exec_payload,
        })
    }
}

impl Ord for ProcessingEVMBlock {
    fn cmp(&self, other: &Self) -> Ordering {
        self.block_num.cmp(&other.block_num)
    }
}

impl PartialOrd for ProcessingEVMBlock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ProcessingEVMBlock {
    fn eq(&self, other: &Self) -> bool {
        self.block_num == other.block_num
    }
}

impl Eq for ProcessingEVMBlock {}

#[cfg(test)]
mod tests {
    use super::*;

    fn evm_hash(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    #[test]
    fn single_reference_vote_cannot_mark_block_noncanonical() {
        let our_hash = evm_hash(0x11);
        let reference_hash = evm_hash(0x22);
        let votes = HashMap::from([(reference_hash, 1)]);

        let validation =
            ProcessingEVMBlock::classify_hash_votes(42, our_hash, 1, 2, &votes, 1, &[]);

        match validation {
            HashValidation::QuorumUnavailable { reason } => {
                assert!(reason.contains("refusing to mark local SHIP hash noncanonical"));
            }
            other => panic!("expected quorum unavailable, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_single_vote_quorum_is_unavailable() {
        let our_hash = evm_hash(0x11);
        let reference_hash = evm_hash(0x22);
        let votes = HashMap::from([(our_hash, 1), (reference_hash, 1)]);

        let validation =
            ProcessingEVMBlock::classify_hash_votes(42, our_hash, 1, 2, &votes, 2, &[]);

        match validation {
            HashValidation::QuorumUnavailable { reason } => {
                assert!(reason.contains("reference RPCs disagree while quorum=1"));
            }
            other => panic!("expected quorum unavailable, got {other:?}"),
        }
    }

    #[test]
    fn two_reference_votes_can_mark_block_noncanonical() {
        let our_hash = evm_hash(0x11);
        let reference_hash = evm_hash(0x22);
        let votes = HashMap::from([(reference_hash, 2)]);

        let validation =
            ProcessingEVMBlock::classify_hash_votes(42, our_hash, 2, 3, &votes, 2, &[]);

        assert_eq!(validation, HashValidation::NonCanonical { reference_hash });
    }
}
