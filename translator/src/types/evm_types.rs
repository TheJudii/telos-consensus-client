use alloy::primitives::aliases::BlockTimestamp;
use alloy::primitives::{Address, Log, B256};
use antelope::chain::asset::Asset;
use antelope::chain::binary_extension::BinaryExtension;
use antelope::chain::checksum::{Checksum160, Checksum256};
use antelope::chain::name::Name;
use antelope::chain::time::TimePoint;
use antelope::chain::Packer;
use antelope::serializer::Decoder;
use antelope::serializer::Encoder;
use antelope::util::hex_to_bytes;
use antelope::StructPacker;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct RawAction {
    pub ram_payer: Name,
    pub tx: Vec<u8>,
    pub estimate_gas: bool,
    pub sender: Option<Checksum160>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct TransferAction {
    pub from: Name,
    pub to: Name,
    pub quantity: Asset,
    pub memo: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct EvmContractConfigRow {
    pub trx_index: u32,
    pub last_block: u32,
    pub gas_used_block: Checksum256,
    pub gas_price: Checksum256,
    pub revision: BinaryExtension<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct AccountRow {
    pub index: u64,
    pub address: Checksum160,
    pub account: Name,
    pub nonce: u64,
    pub code: Vec<u8>,
    pub balance: Checksum256,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct AccountStateRow {
    pub index: u64,
    pub key: Checksum256,
    pub value: Checksum256,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct GlobalTable {
    max_ram_size: u64,
    total_ram_bytes_reserved: u64,
    total_ram_stake: i64,
    last_producer_schedule_update: BlockTimestamp,
    last_proposed_schedule_update: BlockTimestamp,
    last_pervote_bucket_fill: TimePoint,
    pervote_bucket: i64,
    perblock_bucket: i64,
    total_unpaid_blocks: u32,
    total_activated_stake: i64,
    thresh_activated_stake_time: TimePoint,
    last_producer_schedule_size: u16,
    total_producer_vote_weight: f64,
    last_name_close: BlockTimestamp,
    block_num: u32,
    last_claimrewards: u32,
    next_payment: u32,
    new_ram_per_block: u16,
    last_ram_increase: BlockTimestamp,
    last_block_num: BlockTimestamp,
    total_producer_votepay_share: f64,
    revision: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct WithdrawAction {
    pub to: Name,
    pub quantity: Asset,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct SetRevisionAction {
    pub new_revision: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct OpenWalletAction {
    pub account: Name,
    pub address: Checksum160,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, StructPacker)]
pub struct CreateAction {
    pub account: Name,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrintedReceipt {
    pub charged_gas: String,
    pub trx_index: u16,
    pub block: u64,
    pub status: u8,
    pub epoch: u64,
    pub createdaddr: String,
    pub gasused: String,
    #[serde(deserialize_with = "deserialize_logs")]
    pub logs: Vec<Log>,
    pub output: String,
    pub errors: Option<Vec<String>>,
    // pub itxs: any[], // Define struct for this
}

impl Default for PrintedReceipt {
    fn default() -> Self {
        PrintedReceipt {
            charged_gas: "".to_string(),
            trx_index: 0,
            block: 0,
            status: 0,
            epoch: 0,
            createdaddr: "".to_string(),
            gasused: "5208".to_string(),
            logs: vec![],
            output: "".to_string(),
            errors: None,
        }
    }
}

fn deserialize_logs<'de, D>(deserializer: D) -> Result<Vec<Log>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct LogHelper {
        address: String,
        data: String,
        topics: Vec<String>,
    }

    impl LogHelper {
        fn address(&self) -> Address {
            let padded = format!("{:0>40}", self.address);
            padded.parse().expect("Invalid address")
        }
    }

    let log_helpers = Vec::<LogHelper>::deserialize(deserializer)?;
    let mut logs = vec![];
    for log in log_helpers {
        let address = log.address();
        let topics = log
            .topics
            .into_iter()
            .map(|topic| to_b256(&topic))
            .collect();
        let data = log.data.parse().expect("Invalid data");
        logs.push(Log::new(address, topics, data).unwrap());
    }
    Ok(logs)
}

fn to_b256(s: &str) -> B256 {
    let binding = hex_to_bytes(s);
    let b256_slice = binding.as_slice();
    if b256_slice.len() <= 32 {
        B256::left_padding_from(b256_slice)
    } else {
        panic!("Invalid B256 length");
    }
}

impl PrintedReceipt {
    pub fn from_console(console: String) -> Option<Self> {
        let start_pattern = "RCPT{{";
        let end_pattern = "}}RCPT";

        if let Some(start) = console.find(start_pattern) {
            let start_index = start + start_pattern.len();
            if let Some(end) = console[start_index..].find(end_pattern) {
                let end_index = start_index + end;
                let extracted = &console[start_index..end_index];
                match serde_json::from_str::<PrintedReceipt>(extracted) {
                    Ok(printed_receipt) => Some(printed_receipt),
                    Err(e) => {
                        warn!("Failed to parse PrintedReceipt JSON: {} (payload: {})", e, extracted);
                        None
                    }
                }
            } else {
                warn!("End pattern not found in console output.");
                None
            }
        } else {
            // This branch is hit for both genuinely empty consoles and for
            // consoles that contain binary data that resolved to text without
            // the expected RCPT{{...}}RCPT marker. The caller should decide
            // whether to hard-fail or skip.
            debug!(
                "Start pattern not found in console output (console_len={}).",
                console.len()
            );
            None
        }
    }

    pub fn from_rpc_receipt(
        rpc_receipt: &RpcReceipt,
        trx_index: u16,
        block_num: u64,
        block_timestamp: u64,
    ) -> Self {
        let status = if rpc_receipt.status.unwrap_or(false) { 1u8 } else { 0u8 };
        let gasused = format!("{:x}", rpc_receipt.gas_used.unwrap_or(0u128));
        let createdaddr = rpc_receipt
            .contract_address
            .map(|addr| format!("{:x}", addr))
            .unwrap_or_default();

        PrintedReceipt {
            charged_gas: gasused.clone(),
            trx_index,
            block: block_num,
            status,
            epoch: block_timestamp,
            createdaddr,
            gasused,
            logs: rpc_receipt.logs.clone().unwrap_or_default(),
            output: "".to_string(),
            errors: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcReceipt {
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Option<String>,
    #[serde(rename = "blockNumber")]
    pub block_number: Option<String>,
    #[serde(rename = "gasUsed", deserialize_with = "deserialize_gas_used")]
    pub gas_used: Option<u128>,
    #[serde(deserialize_with = "deserialize_status")]
    pub status: Option<bool>,
    #[serde(rename = "contractAddress")]
    pub contract_address: Option<Address>,
    pub logs: Option<Vec<Log>>,
}

fn deserialize_gas_used<'de, D>(deserializer: D) -> Result<Option<u128>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    use serde_json::Value;

    let val = Option::<Value>::deserialize(deserializer)?;
    match val {
        None => Ok(None),
        Some(Value::String(s)) => {
            // Parse hex string like "0xe1e88"
            if s.starts_with("0x") {
                u128::from_str_radix(&s[2..], 16).map(Some).map_err(D::Error::custom)
            } else {
                s.parse::<u128>().map(Some).map_err(D::Error::custom)
            }
        }
        Some(Value::Number(n)) => {
            n.as_u64()
                .map(|u| Some(u as u128))
                .ok_or_else(|| D::Error::custom("invalid gas_used number"))
        }
        Some(_) => Err(D::Error::custom("gas_used must be a string or number")),
    }
}

fn deserialize_status<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    use serde_json::Value;

    let val = Option::<Value>::deserialize(deserializer)?;
    match val {
        None => Ok(None),
        Some(Value::String(s)) => {
            // Parse hex string like "0x1" or "0x0"
            if s.starts_with("0x") {
                let num = u8::from_str_radix(&s[2..], 16).map_err(D::Error::custom)?;
                Ok(Some(num != 0))
            } else {
                s.parse::<u8>()
                    .map(|n| Some(n != 0))
                    .map_err(D::Error::custom)
            }
        }
        Some(Value::Number(n)) => {
            n.as_u64()
                .map(|u| Some(u != 0))
                .ok_or_else(|| D::Error::custom("invalid status number"))
        }
        Some(Value::Bool(b)) => Ok(Some(b)),
        Some(_) => Err(D::Error::custom("status must be a string, number, or boolean")),
    }
}
