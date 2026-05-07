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
use reth_primitives::ReceiptWithBloom;
use reth_telos_rpc_engine_api::structs::TelosEngineAPIExtraFields;
use reth_trie_common::root::ordered_trie_root_with_encoder;
use std::cmp::{max, Ordering};
use std::collections::BTreeMap;
use tracing::{debug, info, warn};

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
    rpc_fallback_endpoint: Option<String>,
    rpc_fallback_sample_every_n: u32,
    block_timestamp: u64,
    /// Count of tx-bearing eosio.evm actions observed during handle_action.
    /// Used to gate the RPC-validation skip path: skip only when this is 0
    /// (genuine-empty evidence), not when self.transactions is empty
    /// (which can falsely mean "we dropped them silently"). See block.rs
    /// commentary around the skip path for full rationale.
    pub tx_actions_seen: u32,
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
    pub rpc_fallback_endpoint: Option<String>,
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
            rpc_fallback_endpoint,
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
            rpc_fallback_endpoint,
            rpc_fallback_sample_every_n,
            block_timestamp,
            tx_actions_seen: 0,
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

    async fn fetch_receipt_from_rpc(
        &self,
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

    async fn handle_action(
        &mut self,
        action: Box<dyn BasicTrace + Send>,
        native_to_evm_cache: &NameToAddressCache,
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
            // and rpc_fallback_endpoint is configured, fetch the receipt from the
            // RPC endpoint using eth_getTransactionReceipt.
            let raw_console_bytes = action.raw_console();
            let printed_receipt = match PrintedReceipt::from_console(action.console()) {
                Some(r) => r,
                None => {
                    // Try RPC fallback if configured
                    if let Some(rpc_endpoint) = &self.rpc_fallback_endpoint {
                        // Compute tx hash from raw tx bytes
                        let tx_hash = keccak256(&raw.tx);
                        let tx_hash_str = format!("0x{}", hex::encode(tx_hash));

                        match self
                            .fetch_receipt_from_rpc(rpc_endpoint, &tx_hash_str)
                            .await
                        {
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
                                // DO NOT silently skip. Earlier versions assumed
                                // "RPC returned null => prod skipped it" but this
                                // is wrong in the face of RPC replica lag or eventual
                                // consistency (observed on mainnet-quick at block
                                // 463,899,714: tx 0x53e1e7cf returned null during
                                // initial sync but exists on-chain). Silent skip
                                // produces an empty-block hash that diverges from
                                // canonical and gets committed to reth without
                                // verification when the block happens to not be
                                // a sample block. Fail loud instead — the outer
                                // handler at block.rs ~790 will catch this as an
                                // action-processing error and fall back to
                                // whole-block RPC fetch.
                                warn!(
                                    "Single-tx receipt RPC returned null for tx {} in                                      block {}. Promoting to block-level fallback.",
                                    tx_hash_str, self.block_num
                                );
                                return Err(eyre::eyre!(
                                    "Single-tx receipt null for tx {} in block {}                                      (likely RPC replica lag); triggering block-level fallback",
                                    tx_hash_str, self.block_num
                                ));
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
                             Configure rpc_fallback_endpoint to use RPC fallback.",
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

    /// Fetch a full block from the reference RPC and reconstruct the execution payload.
    /// Used when SHIP data is incomplete (missing action traces, console output, etc.)
    /// and the locally-built block hash doesn't match production.
    async fn fetch_block_from_rpc(
        &self,
        rpc_endpoint: &str,
        evm_block_num: u64,
        expected_parent_hash: B256,
    ) -> eyre::Result<(Header, ExecutionPayloadV1)> {
        let block_hex = format!("0x{:x}", evm_block_num);

        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": [block_hex, true],
            "id": 1,
        });

        use std::sync::LazyLock;
        static RPC_CLIENT_BLOCK: LazyLock<reqwest::Client> = LazyLock::new(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(4)
                .build()
                .unwrap_or_default()
        });

        let response = RPC_CLIENT_BLOCK
            .post(rpc_endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| eyre!("Failed to fetch block from RPC: {}", e))?;

        let json_response: serde_json::Value = response
            .json()
            .await
            .map_err(|e| eyre!("Failed to parse block RPC response: {}", e))?;

        let block_data = json_response
            .get("result")
            .ok_or_else(|| eyre!("No result in block RPC response"))?;

        if block_data.is_null() {
            return Err(eyre!("Block {} not found on reference RPC", evm_block_num));
        }

        // Parse the block fields we need
        let block_hash_str = block_data["hash"]
            .as_str()
            .ok_or_else(|| eyre!("No hash in block"))?;
        let block_hash: B256 = block_hash_str
            .parse()
            .map_err(|e| eyre!("Failed to parse block hash: {}", e))?;

        let parent_hash_str = block_data["parentHash"]
            .as_str()
            .ok_or_else(|| eyre!("No parentHash in block"))?;
        let parent_hash: B256 = parent_hash_str
            .parse()
            .map_err(|e| eyre!("Failed to parse parent hash: {}", e))?;
        if parent_hash != expected_parent_hash {
            warn!(
                "RPC block {} parent differs from local parent: rpc={} local={}",
                evm_block_num, parent_hash, expected_parent_hash
            );
        }

        let gas_used_str = block_data["gasUsed"].as_str().unwrap_or("0x0");
        let gas_used = u64::from_str_radix(gas_used_str.trim_start_matches("0x"), 16).unwrap_or(0);

        let gas_limit_str = block_data["gasLimit"].as_str().unwrap_or("0x7fffffff");
        let gas_limit = u64::from_str_radix(gas_limit_str.trim_start_matches("0x"), 16)
            .unwrap_or(DEFAULT_GAS_LIMIT as u64);

        let timestamp_str = block_data["timestamp"].as_str().unwrap_or("0x0");
        let timestamp =
            u64::from_str_radix(timestamp_str.trim_start_matches("0x"), 16).unwrap_or(0);

        let extra_data_str = block_data["extraData"].as_str().unwrap_or("0x");
        let extra_data = Bytes::from(
            alloy::primitives::hex::decode(extra_data_str.trim_start_matches("0x"))
                .unwrap_or_default(),
        );

        let tx_root_str = block_data["transactionsRoot"]
            .as_str()
            .unwrap_or("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");
        let tx_root: B256 = tx_root_str.parse().unwrap_or(EMPTY_ROOT_HASH);

        let receipts_root_str = block_data["receiptsRoot"]
            .as_str()
            .unwrap_or("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");
        let receipts_root: B256 = receipts_root_str.parse().unwrap_or(EMPTY_ROOT_HASH);

        let logs_bloom_str = block_data["logsBloom"].as_str().unwrap_or("0x00");
        let logs_bloom_bytes =
            alloy::primitives::hex::decode(logs_bloom_str.trim_start_matches("0x"))
                .unwrap_or_default();
        let logs_bloom = Bloom::from_slice(&logs_bloom_bytes);

        // Collect raw transaction bytes
        let txs_array = block_data["transactions"].as_array();
        let mut transactions: Vec<Bytes> = Vec::new();

        if let Some(txs) = txs_array {
            for tx_val in txs {
                // For full tx objects, we need to fetch the raw tx
                if let Some(tx_hash_str) = tx_val["hash"].as_str() {
                    // Fetch raw transaction via eth_getRawTransactionByHash
                    let raw_tx_request = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_getRawTransactionByHash",
                        "params": [tx_hash_str],
                        "id": 1,
                    });
                    let raw_response = RPC_CLIENT_BLOCK
                        .post(rpc_endpoint)
                        .json(&raw_tx_request)
                        .send()
                        .await
                        .map_err(|e| eyre!("Failed to fetch raw tx: {}", e))?;
                    let raw_json: serde_json::Value = raw_response
                        .json()
                        .await
                        .map_err(|e| eyre!("Failed to parse raw tx response: {}", e))?;
                    if let Some(raw_hex) = raw_json["result"].as_str() {
                        let raw_bytes =
                            alloy::primitives::hex::decode(raw_hex.trim_start_matches("0x"))
                                .map_err(|e| eyre!("Failed to decode raw tx hex: {}", e))?;
                        transactions.push(Bytes::from(raw_bytes));
                    }
                }
            }
        }

        let base_fee_per_gas = U256::from(MINIMUM_FEE_PER_GAS);

        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Default::default(),
            state_root: EMPTY_ROOT_HASH,
            transactions_root: tx_root,
            receipts_root: receipts_root,
            withdrawals_root: None,
            logs_bloom,
            difficulty: Default::default(),
            number: evm_block_num,
            gas_limit: gas_limit as u128,
            gas_used: gas_used as u128,
            timestamp,
            mix_hash: Default::default(),
            nonce: Default::default(),
            base_fee_per_gas: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_root: None,
            extra_data: extra_data.clone(),
        };

        let exec_payload = ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Default::default(),
            state_root: EMPTY_ROOT_HASH,
            receipts_root,
            logs_bloom,
            prev_randao: B256::ZERO,
            block_number: evm_block_num,
            gas_limit,
            gas_used,
            timestamp,
            extra_data,
            base_fee_per_gas,
            block_hash,
            transactions,
        };

        Ok((header, exec_payload))
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
            let mut action_processing_failed = false;

            for TransactionTrace::V0(t) in traces {
                for action in t.action_traces {
                    if let Err(e) = self
                        .handle_action(Box::new(action), native_to_evm_cache)
                        .await
                    {
                        let evm_block_num = (self.block_num - block_delta) as u64;
                        warn!(
                            "Error handling action in block {} (EVM {}): {}. Will try RPC fallback.",
                            self.block_num, evm_block_num, e
                        );
                        action_processing_failed = true;
                        break;
                    }
                }
                if action_processing_failed {
                    break;
                }
            }

            // If action processing failed and we have an RPC fallback, fetch the
            // block directly from the reference chain
            if action_processing_failed {
                if let Some(rpc_endpoint) = &self.rpc_fallback_endpoint {
                    let evm_block_num = (self.block_num - block_delta) as u64;
                    match self
                        .fetch_block_from_rpc(rpc_endpoint, evm_block_num, parent_hash)
                        .await
                    {
                        Ok(rpc_result) => {
                            info!(
                                "Block {} (EVM {}) recovered from RPC after action error (hash={})",
                                self.block_num, evm_block_num, rpc_result.1.block_hash
                            );
                            return Ok(GeneratedEvmData::Canonical {
                                header: rpc_result.0,
                                execution_payload: rpc_result.1,
                            });
                        }
                        Err(e) => {
                            return Err(eyre!(
                                "Action processing failed and RPC fallback also failed for block {} (EVM {}): {}",
                                self.block_num, evm_block_num, e
                            ));
                        }
                    }
                } else {
                    return Err(eyre!(
                        "Action processing failed for block {} and no RPC fallback configured",
                        self.block_num
                    ));
                }
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
        if let Some(rpc_endpoint) = &self.rpc_fallback_endpoint {
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
                    "Block {}: observed {} tx-bearing actions but produced 0 output txs.                      Forcing RPC validation (silent-drop suspected).",
                    evm_block_num, self.tx_actions_seen
                );
            }
            if had_no_tx_actions && !is_sample_block {
                return Ok(GeneratedEvmData::Canonical {
                    header,
                    execution_payload: exec_payload,
                });
            }

            let block_hex = format!("0x{:x}", evm_block_num);

            // Quick hash check against reference RPC
            // Use a static client to reuse TCP connections across blocks
            use std::sync::LazyLock;
            static RPC_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
                reqwest::Client::builder()
                    .pool_max_idle_per_host(4)
                    .build()
                    .unwrap_or_default()
            });

            let hash_request = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getBlockByNumber",
                "params": [block_hex, false],
                "id": 1,
            });

            let response = RPC_CLIENT
                .post(rpc_endpoint)
                .json(&hash_request)
                .send()
                .await
                .map_err(|e| {
                    eyre!(
                        "Block {} canonical hash verification failed; refusing to trust local SHIP data: {}",
                        evm_block_num,
                        e
                    )
                })?;

            let json_response = response.json::<serde_json::Value>().await.map_err(|e| {
                eyre!(
                    "Block {} canonical hash response was not valid JSON; refusing to trust local SHIP data: {}",
                    evm_block_num,
                    e
                )
            })?;

            let result = json_response.get("result").ok_or_else(|| {
                eyre!(
                    "Block {} canonical hash response had no result",
                    evm_block_num
                )
            })?;
            if result.is_null() {
                return Err(eyre!(
                    "Block {} is not available from canonical RPC; refusing to trust local SHIP data",
                    evm_block_num
                ));
            }

            let ref_hash_str = result["hash"].as_str().ok_or_else(|| {
                eyre!(
                    "Block {} canonical hash response had no hash",
                    evm_block_num
                )
            })?;
            let ref_hash = ref_hash_str
                .parse::<B256>()
                .map_err(|e| eyre!("Block {} canonical hash was invalid: {}", evm_block_num, e))?;

            if our_hash != ref_hash {
                warn!(
                    "Block {} is not canonical: local SHIP hash={} reference={}. Skipping until SHIP provides the canonical fork.",
                    evm_block_num, our_hash, ref_hash
                );
                return Ok(GeneratedEvmData::NonCanonical {
                    evm_block_num: evm_block_num as u32,
                    local_hash: our_hash,
                    reference_hash: ref_hash,
                });
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
