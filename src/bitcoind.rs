use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::hashes::hex::FromHex;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::log_error;
use lightning::routing::utxo::{UtxoLookup, UtxoResult};
use lightning::util::logger::Logger;
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::http::JsonResponse;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::disk::FilesystemLogger;

pub struct BitcoindClient {
    bitcoind_rpc_client: Arc<RpcClient>,
    fees: Arc<HashMap<Target, AtomicU32>>,
    handle: tokio::runtime::Handle,
    logger: Arc<FilesystemLogger>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub enum Target {
    Background,
    Normal,
    HighPriority,
}

impl BlockSource for BitcoindClient {
    fn get_header<'a>(
        &'a self,
        header_hash: &'a BlockHash,
        height_hint: Option<u32>,
    ) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
        Box::pin(async move {
            self.bitcoind_rpc_client
                .get_header(header_hash, height_hint)
                .await
        })
    }

    fn get_block<'a>(
        &'a self,
        header_hash: &'a BlockHash,
    ) -> AsyncBlockSourceResult<'a, BlockData> {
        Box::pin(async move { self.bitcoind_rpc_client.get_block(header_hash).await })
    }

    fn get_best_block(&self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
        Box::pin(async move { self.bitcoind_rpc_client.get_best_block().await })
    }
}

pub struct BlockchainInfo {
    pub latest_height: usize,
    pub latest_blockhash: BlockHash,
    pub chain: String,
}

impl TryInto<BlockchainInfo> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<BlockchainInfo> {
        Ok(BlockchainInfo {
            latest_height: self.0["blocks"].as_u64().unwrap() as usize,
            latest_blockhash: BlockHash::from_hex(self.0["bestblockhash"].as_str().unwrap())
                .unwrap(),
            chain: self.0["chain"].as_str().unwrap().to_string(),
        })
    }
}

pub struct FeeResponse {
    pub feerate_sat_per_kw: Option<u32>,
    pub errored: bool,
}

impl TryInto<FeeResponse> for JsonResponse {
    type Error = std::io::Error;
    fn try_into(self) -> std::io::Result<FeeResponse> {
        let errored = !self.0["errors"].is_null();
        Ok(FeeResponse {
            errored,
            feerate_sat_per_kw: self.0["feerate"].as_f64().map(|feerate_btc_per_kvbyte| {
                (feerate_btc_per_kvbyte * 100_000_000.0 / 4.0).round() as u32
            }),
        })
    }
}

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

impl BitcoindClient {
    pub(crate) async fn new(
        host: String,
        port: u16,
        rpc_user: String,
        rpc_password: String,
        handle: tokio::runtime::Handle,
        logger: Arc<FilesystemLogger>,
    ) -> std::io::Result<Self> {
        let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
        let rpc_credentials =
            base64::encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));
        let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
        let _dummy = bitcoind_rpc_client
            .call_method::<BlockchainInfo>("getblockchaininfo", &[])
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied,
				"failed to make initial call to bitcoind - please check your RPC user/password and access settings")
            })?;
        let mut fees: HashMap<Target, AtomicU32> = HashMap::new();
        fees.insert(Target::Background, AtomicU32::new(MIN_FEERATE));
        fees.insert(Target::Normal, AtomicU32::new(2000));
        fees.insert(Target::HighPriority, AtomicU32::new(5000));
        let client = Self {
            bitcoind_rpc_client: Arc::new(bitcoind_rpc_client),
            fees: Arc::new(fees),
            handle: handle.clone(),
            logger,
        };
        BitcoindClient::poll_for_fee_estimates(
            client.fees.clone(),
            client.bitcoind_rpc_client.clone(),
            handle,
        );
        Ok(client)
    }

    fn poll_for_fee_estimates(
        fees: Arc<HashMap<Target, AtomicU32>>,
        rpc_client: Arc<RpcClient>,
        handle: tokio::runtime::Handle,
    ) {
        handle.spawn(async move {
            loop {
                let background_estimate = {
                    let background_conf_target = serde_json::json!(144);
                    let background_estimate_mode = serde_json::json!("ECONOMICAL");
                    let resp = rpc_client
                        .call_method::<FeeResponse>(
                            "estimatesmartfee",
                            &[background_conf_target, background_estimate_mode],
                        )
                        .await
                        .unwrap();
                    match resp.feerate_sat_per_kw {
                        Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
                        None => MIN_FEERATE,
                    }
                };

                let normal_estimate = {
                    let normal_conf_target = serde_json::json!(18);
                    let normal_estimate_mode = serde_json::json!("ECONOMICAL");
                    let resp = rpc_client
                        .call_method::<FeeResponse>(
                            "estimatesmartfee",
                            &[normal_conf_target, normal_estimate_mode],
                        )
                        .await
                        .unwrap();
                    match resp.feerate_sat_per_kw {
                        Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
                        None => 2000,
                    }
                };

                let high_prio_estimate = {
                    let high_prio_conf_target = serde_json::json!(6);
                    let high_prio_estimate_mode = serde_json::json!("CONSERVATIVE");
                    let resp = rpc_client
                        .call_method::<FeeResponse>(
                            "estimatesmartfee",
                            &[high_prio_conf_target, high_prio_estimate_mode],
                        )
                        .await
                        .unwrap();

                    match resp.feerate_sat_per_kw {
                        Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
                        None => 5000,
                    }
                };

                fees.get(&Target::Background)
                    .unwrap()
                    .store(background_estimate, Ordering::Release);
                fees.get(&Target::Normal)
                    .unwrap()
                    .store(normal_estimate, Ordering::Release);
                fees.get(&Target::HighPriority)
                    .unwrap()
                    .store(high_prio_estimate, Ordering::Release);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    pub async fn get_blockchain_info(&self) -> BlockchainInfo {
        self.bitcoind_rpc_client
            .call_method::<BlockchainInfo>("getblockchaininfo", &[])
            .await
            .unwrap()
    }
}

impl FeeEstimator for BitcoindClient {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        match confirmation_target {
            ConfirmationTarget::Background => self
                .fees
                .get(&Target::Background)
                .unwrap()
                .load(Ordering::Acquire),
            ConfirmationTarget::Normal => self
                .fees
                .get(&Target::Normal)
                .unwrap()
                .load(Ordering::Acquire),
            ConfirmationTarget::HighPriority => self
                .fees
                .get(&Target::HighPriority)
                .unwrap()
                .load(Ordering::Acquire),
        }
    }
}

impl BroadcasterInterface for BitcoindClient {
    fn broadcast_transaction(&self, tx: &Transaction) {
        let bitcoind_rpc_client = self.bitcoind_rpc_client.clone();
        let tx_serialized = encode::serialize_hex(tx);
        let tx_json = serde_json::json!(tx_serialized);
        let logger = Arc::clone(&self.logger);
        self.handle.spawn(async move {
			// This may error due to RL calling `broadcast_transaction` with the same transaction
			// multiple times, but the error is safe to ignore.
			match bitcoind_rpc_client
				.call_method::<Txid>("sendrawtransaction", &[tx_json])
				.await
			{
				Ok(_) => {}
				Err(e) => {
					let err_str = e.get_ref().unwrap().to_string();
					log_error!(logger,
						"Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\nTransaction: {}",
						err_str,
						tx_serialized);
					tracing::warn!("Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\n> ", err_str);
				}
			}
		});
    }
}

impl UtxoLookup for BitcoindClient {
    fn get_utxo(&self, _genesis_hash: &BlockHash, _short_channel_id: u64) -> UtxoResult {
        // P2PGossipSync takes None for a UtxoLookup, so this will never be called.
        todo!();
    }
}
