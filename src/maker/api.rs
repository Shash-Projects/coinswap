//! The Maker API.
//!
//! Defines the core functionality of the Maker in a swap protocol implementation.
//! It includes structures for managing maker behavior, connection states, and recovery from swap events.
//! The module provides methods for initializing a Maker, verifying swap messages, and monitoring
//! contract broadcasts and handle idle Taker connections. Additionally, it handles recovery by broadcasting
//! contract transactions and claiming funds after an unsuccessful swap event.

use crate::{
    protocol::{
        contract::check_hashvalues_are_equal,
        messages::{FidelityProof, ReqContractSigsForSender},
        Hash160,
    },
    utill::{get_maker_dir, redeemscript_to_scriptpubkey, ConnectionType},
    wallet::{RPCConfig, SwapCoin, WalletSwapCoin},
};
use bitcoin::{
    ecdsa::Signature,
    secp256k1::{self, Secp256k1},
    OutPoint, PublicKey, ScriptBuf, Transaction, locktime::relative::LockTime
};

use bitcoind::bitcoincore_rpc::RpcApi;
use std::{
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex, RwLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    protocol::{
        contract::{
            check_hashlock_has_pubkey, check_multisig_has_pubkey, check_reedemscript_is_multisig,
            find_funding_output_index, read_contract_locktime,
        },
        messages::ProofOfFunding,
    },
    wallet::{IncomingSwapCoin, OutgoingSwapCoin, Wallet, WalletError},
};

use super::{config::MakerConfig, error::MakerError};

use crate::maker::server::{
    HEART_BEAT_INTERVAL_SECS, MIN_CONTRACT_REACTION_TIME, REQUIRED_CONFIRMS,
};

/// Used to configure the maker for testing purposes.
#[derive(Debug, Clone, Copy)]
pub enum MakerBehavior {
    Normal,
    CloseAtReqContractSigsForSender,
    CloseAtProofOfFunding,
    CloseAtContractSigsForRecvrAndSender,
    CloseAtContractSigsForRecvr,
    CloseAtHashPreimage,
    BroadcastContractAfterSetup,
}

/// Expected messages for the taker in the context of [ConnectionState] structure.
///
/// If the received message doesn't match expected message,
/// a protocol error will be returned.
#[derive(Debug, Default, PartialEq, Clone)]
pub enum ExpectedMessage {
    #[default]
    TakerHello,
    NewlyConnectedTaker,
    ReqContractSigsForSender,
    ProofOfFunding,
    ProofOfFundingORContractSigsForRecvrAndSender,
    ReqContractSigsForRecvr,
    HashPreimage,
    PrivateKeyHandover,
}

/// Maintains the state of a connection, including the list of swapcoins and the next expected message.
#[derive(Debug, Default, Clone)]
pub struct ConnectionState {
    pub allowed_message: ExpectedMessage,
    pub incoming_swapcoins: Vec<IncomingSwapCoin>,
    pub outgoing_swapcoins: Vec<OutgoingSwapCoin>,
    pub pending_funding_txes: Vec<Transaction>,
}

pub struct ThreadPool {
    pub threads: Mutex<Vec<JoinHandle<()>>>,
    pub port: u16,
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        if let Err(e) = self.join_all_threads() {
            log::error!("Error joining threads in via drop: {:?}", e);
        }
    }
}

impl ThreadPool {
    pub fn new(port: u16) -> Self {
        Self {
            threads: Mutex::new(Vec::new()),
            port,
        }
    }

    pub fn add_thread(&self, handle: JoinHandle<()>) {
        let mut threads = self.threads.lock().unwrap();
        threads.push(handle);
    }
    #[inline]
    fn join_all_threads(&self) -> Result<(), MakerError> {
        let mut threads = self
            .threads
            .lock()
            .map_err(|_| MakerError::General("Failed to lock threads"))?;

        log::info!("Joining {} threads", threads.len());

        let mut joined_count = 0;
        while let Some(thread) = threads.pop() {
            let thread_name = thread.thread().name().unwrap().to_string();

            match thread.join() {
                Ok(_) => {
                    log::info!("[{}] Thread {} joined", self.port, thread_name);
                    joined_count += 1;
                }
                Err(e) => {
                    log::error!(
                        "[{}] Error {:?} while joining thread {}",
                        self.port,
                        e,
                        thread_name
                    );
                }
            }
        }

        log::info!("Successfully joined {} threads", joined_count,);
        Ok(())
    }
}

/// Represents the maker in the swap protocol.
pub struct Maker {
    /// Defines special maker behavior, only applicable for testing
    pub behavior: MakerBehavior,
    /// Maker configurations
    pub config: MakerConfig,
    /// Maker's underlying wallet
    pub wallet: RwLock<Wallet>,
    /// A flag to trigger shutdown event
    pub shutdown: AtomicBool,
    /// Map of IP address to Connection State + last Connected instant
    pub connection_state: Mutex<HashMap<IpAddr, (ConnectionState, Instant)>>,
    /// Highest Value Fidelity Proof
    pub highest_fidelity_proof: RwLock<Option<FidelityProof>>,
    /// Is setup complete
    pub is_setup_complete: AtomicBool,
    /// Path for the data directory.
    pub data_dir: PathBuf,
    /// Thread pool for managing all spawned threads
    pub thread_pool: Arc<ThreadPool>,
}

#[allow(clippy::too_many_arguments)]
impl Maker {
    /// Initializes a Maker structure.
    ///
    /// This function sets up a Maker instance with configurable parameters.  
    /// It handles the initialization of data directories, wallet files, and RPC configurations.
    ///
    /// ### Parameters:
    /// - `data_dir`:  
    ///   - `Some(value)`: Use the specified directory for storing data.  
    ///   - `None`: Use the default data directory (e.g., for Linux: `~/.coinswap/maker`).  
    /// - `wallet_file_name`:  
    ///   - `Some(value)`: Attempt to load a wallet file named `value`. If it does not exist, a new wallet with the given name will be created.  
    ///   - `None`: Create a new wallet file with the default name `maker-wallet`.  
    /// - If `rpc_config` = `None`: Use the default [`RPCConfig`]
    pub fn init(
        data_dir: Option<PathBuf>,
        wallet_file_name: Option<String>,
        rpc_config: Option<RPCConfig>,
        port: Option<u16>,
        rpc_port: Option<u16>,
        socks_port: Option<u16>,
        connection_type: Option<ConnectionType>,
        behavior: MakerBehavior,
    ) -> Result<Self, MakerError> {
        // Get provided data directory or the default data directory.
        let data_dir = data_dir.unwrap_or(get_maker_dir());
        let wallets_dir = data_dir.join("wallets");

        // Use the provided name or default to `maker-wallet` if not specified.
        let wallet_file_name = wallet_file_name.unwrap_or_else(|| "maker-wallet".to_string());
        let wallet_path = wallets_dir.join(&wallet_file_name);

        let mut rpc_config = rpc_config.unwrap_or_default();

        rpc_config.wallet_name = wallet_file_name;

        let mut wallet = if wallet_path.exists() {
            // wallet already exists , load the wallet
            let wallet = Wallet::load(&wallet_path, &rpc_config)?;
            log::info!("Wallet file at {:?} successfully loaded.", wallet_path);
            wallet
        } else {
            // wallet doesn't exists at the given path , create a new one
            let wallet = Wallet::init(&wallet_path, &rpc_config)?;
            log::info!("New Wallet created at : {:?}", wallet_path);
            wallet
        };

        // If config file doesn't exist, default config will be loaded.
        let mut config = MakerConfig::new(Some(&data_dir.join("config.toml")))?;

        if let Some(port) = port {
            config.port = port;
        }

        if let Some(rpc_port) = rpc_port {
            config.rpc_port = rpc_port;
        }

        if let Some(socks_port) = socks_port {
            config.socks_port = socks_port;
        }

        if let Some(connection_type) = connection_type {
            config.connection_type = connection_type;
        }

        let port = config.port;

        config.write_to_file(&data_dir.join("config.toml"))?;

        log::info!("Initializing wallet sync");
        wallet.sync()?;
        log::info!("Completed wallet sync");

        Ok(Self {
            behavior,
            config,
            wallet: RwLock::new(wallet),
            shutdown: AtomicBool::new(false),
            connection_state: Mutex::new(HashMap::new()),
            highest_fidelity_proof: RwLock::new(None),
            is_setup_complete: AtomicBool::new(false),
            data_dir,
            thread_pool: Arc::new(ThreadPool::new(port)),
        })
    }

    pub fn get_data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Returns a reference to the Maker's wallet.
    pub fn get_wallet(&self) -> &RwLock<Wallet> {
        &self.wallet
    }

    /// Checks consistency of the [ProofOfFunding] message and return the Hashvalue
    /// used in hashlock transaction.
    pub fn verify_proof_of_funding(&self, message: &ProofOfFunding) -> Result<Hash160, MakerError> {
        if message.confirmed_funding_txes.is_empty() {
            return Err(MakerError::General("No funding txs provided by Taker"));
        }

        for funding_info in &message.confirmed_funding_txes {
            // check that the new locktime is sufficently short enough compared to the
            // locktime in the provided funding tx
            let locktime:locktime::relative::LockTime = read_contract_locktime(&funding_info.contract_redeemscript)?;
            if locktime - message.next_locktime < MIN_CONTRACT_REACTION_TIME {
                return Err(MakerError::General(
                    "Next hop locktime too close to current hop locktime",
                ));
            }

            let funding_output_index = find_funding_output_index(funding_info)?;

            //check the funding_tx is confirmed to required depth
            if let Some(txout) = self
                .wallet
                .read()?
                .rpc
                .get_tx_out(
                    &funding_info.funding_tx.compute_txid(),
                    funding_output_index,
                    None,
                )
                .map_err(WalletError::Rpc)?
            {
                if txout.confirmations < (REQUIRED_CONFIRMS as u32) {
                    return Err(MakerError::General(
                        "funding tx not confirmed to required depth",
                    ));
                }
            } else {
                return Err(MakerError::General("funding tx output doesnt exist"));
            }

            check_reedemscript_is_multisig(&funding_info.multisig_redeemscript)?;

            let (_, tweabale_pubkey) = self.wallet.read()?.get_tweakable_keypair()?;

            check_multisig_has_pubkey(
                &funding_info.multisig_redeemscript,
                &tweabale_pubkey,
                &funding_info.multisig_nonce,
            )?;

            check_hashlock_has_pubkey(
                &funding_info.contract_redeemscript,
                &tweabale_pubkey,
                &funding_info.hashlock_nonce,
            )?;

            //check that the provided contract matches the scriptpubkey from the
            //cache which was populated when the ReqContractSigsForSender message arrived
            let contract_spk = redeemscript_to_scriptpubkey(&funding_info.contract_redeemscript)?;

            if !self.wallet.read()?.does_prevout_match_cached_contract(
                &(OutPoint {
                    txid: funding_info.funding_tx.compute_txid(),
                    vout: funding_output_index,
                }),
                &contract_spk,
            )? {
                return Err(MakerError::General(
                    "provided contract does not match sender contract tx, rejecting",
                ));
            }
        }

        Ok(check_hashvalues_are_equal(message)?)
    }

    /// Verify the contract transaction for Sender and return the signatures.
    pub fn verify_and_sign_contract_tx(
        &self,
        message: &ReqContractSigsForSender,
    ) -> Result<Vec<Signature>, MakerError> {
        let mut sigs = Vec::<Signature>::new();
        for txinfo in &message.txs_info {
            if txinfo.senders_contract_tx.input.len() != 1
                || txinfo.senders_contract_tx.output.len() != 1
            {
                return Err(MakerError::General(
                    "invalid number of inputs or outputs in contract transaction",
                ));
            }

            if !self.wallet.read()?.does_prevout_match_cached_contract(
                &txinfo.senders_contract_tx.input[0].previous_output,
                &txinfo.senders_contract_tx.output[0].script_pubkey,
            )? {
                return Err(MakerError::General(
                    "taker attempting multiple contract attack, rejecting",
                ));
            }

            let (tweakable_privkey, tweakable_pubkey) =
                self.wallet.read()?.get_tweakable_keypair()?;

            check_multisig_has_pubkey(
                &txinfo.multisig_redeemscript,
                &tweakable_pubkey,
                &txinfo.multisig_nonce,
            )?;

            let secp = Secp256k1::new();

            let hashlock_privkey = tweakable_privkey.add_tweak(&txinfo.hashlock_nonce.into())?;

            let hashlock_pubkey = PublicKey {
                compressed: true,
                inner: secp256k1::PublicKey::from_secret_key(&secp, &hashlock_privkey),
            };

            crate::protocol::contract::is_contract_out_valid(
                &txinfo.senders_contract_tx.output[0],
                &hashlock_pubkey,
                &txinfo.timelock_pubkey,
                &message.hashvalue,
                &message.locktime,
                &MIN_CONTRACT_REACTION_TIME,
            )?;

            self.wallet.write()?.cache_prevout_to_contract(
                txinfo.senders_contract_tx.input[0].previous_output,
                txinfo.senders_contract_tx.output[0].script_pubkey.clone(),
            )?;

            let multisig_privkey = tweakable_privkey.add_tweak(&txinfo.multisig_nonce.into())?;

            let sig = crate::protocol::contract::sign_contract_tx(
                &txinfo.senders_contract_tx,
                &txinfo.multisig_redeemscript,
                txinfo.funding_input_value,
                &multisig_privkey,
            )?;
            sigs.push(sig);
        }
        Ok(sigs)
    }
}

/// Constantly checks for contract transactions in the bitcoin network for all
/// unsettled swap.
///
/// If any one of the is ever observed, run the recovery routine.
pub fn check_for_broadcasted_contracts(maker: Arc<Maker>) -> Result<(), MakerError> {
    let mut failed_swap_ip = Vec::new();
    loop {
        if maker.shutdown.load(Relaxed) {
            break;
        }
        // An extra scope to release all locks when done.
        {
            let mut lock_onstate = maker.connection_state.lock()?;
            for (ip, (connection_state, _)) in lock_onstate.iter_mut() {
                let txids_to_watch = connection_state
                    .incoming_swapcoins
                    .iter()
                    .map(|is| is.contract_tx.compute_txid())
                    .chain(
                        connection_state
                            .outgoing_swapcoins
                            .iter()
                            .map(|oc| oc.contract_tx.compute_txid()),
                    )
                    .collect::<Vec<_>>();

                // No need to check for other contracts in the connection state, if any one of them
                // is ever observed in the mempool/block, run recovery routine.
                for txid in txids_to_watch {
                    if maker
                        .wallet
                        .read()?
                        .rpc
                        .get_raw_transaction_info(&txid, None)
                        .is_ok()
                    {
                        let mut outgoings = Vec::new();
                        let mut incomings = Vec::new();
                        // Something is broadcasted. Report, Recover and Abort.
                        log::warn!(
                            "[{}] Contract txs broadcasted!! txid: {} Recovering from ongoing swaps.",
                            maker.config.port,
                            txid
                        );
                        // Extract Incoming and Outgoing contracts, and timelock spends of the contract transactions.
                        // fully signed.
                        for (og_sc, ic_sc) in connection_state
                            .outgoing_swapcoins
                            .iter()
                            .zip(connection_state.incoming_swapcoins.iter())
                        {
                            let contract_timelock = og_sc.get_timelock()?;
                            let next_internal_address =
                                &maker.wallet.read()?.get_next_internal_addresses(1)?[0];
                            let time_lock_spend =
                                og_sc.create_timelock_spend(next_internal_address)?;

                            // Sometimes we might not have other's contact signatures.
                            // This means the protocol have been stopped abruptly.
                            // This needs more careful consideration as this should not happen
                            // after funding transactions have been broadcasted for outgoing contracts.
                            // For incomings, its less lethal as thats mostly the other party's burden.
                            if let Ok(tx) = og_sc.get_fully_signed_contract_tx() {
                                outgoings.push((
                                    (og_sc.get_multisig_redeemscript(), tx),
                                    (contract_timelock, time_lock_spend),
                                ));
                            } else {
                                log::warn!(
                                    "[{}] Outgoing contact signature not known. Not Broadcasting",
                                    maker.config.port
                                );
                            }
                            if let Ok(tx) = ic_sc.get_fully_signed_contract_tx() {
                                incomings.push((ic_sc.get_multisig_redeemscript(), tx));
                            } else {
                                log::warn!(
                                    "[{}] Incoming contact signature not known. Not Broadcasting",
                                    maker.config.port
                                );
                            }
                        }
                        failed_swap_ip.push(*ip);

                        // Spawn a separate thread to wait for contract maturity and broadcasting timelocked.
                        let maker_clone = maker.clone();
                        log::info!(
                            "[{}] Spawning recovery thread after seeing contracts in mempool",
                            maker.config.port
                        );
                        let handle = std::thread::Builder::new()
                            .name("Swap recovery thread".to_string())
                            .spawn(move || {
                                if let Err(e) = recover_from_swap(maker_clone, outgoings, incomings)
                                {
                                    log::error!("Failed to recover from swap due to: {:?}", e);
                                }
                            })?;
                        maker.thread_pool.add_thread(handle);
                        // Clear the state value here
                        *connection_state = ConnectionState::default();
                        break;
                    }
                }
            }

            // Clear the state entry here
            for ip in failed_swap_ip.iter() {
                lock_onstate.remove(ip);
            }
        } // All locks are cleared here.

        std::thread::sleep(Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
    }

    Ok(())
}

/// Check that if any Taker connection went idle.
///
/// If a connection remains idle for more than idle timeout time, thats a potential DOS attack.
/// Broadcast the contract transactions and claim funds via timelock.
pub fn check_for_idle_states(maker: Arc<Maker>) -> Result<(), MakerError> {
    let mut bad_ip = Vec::new();
    loop {
        if maker.shutdown.load(Relaxed) {
            break;
        }
        let current_time = Instant::now();

        // Extra scope to release all locks when done.
        {
            let mut lock_on_state = maker.connection_state.lock()?;
            for (ip, (state, last_connected_time)) in lock_on_state.iter_mut() {
                let mut outgoings = Vec::new();
                let mut incomings = Vec::new();

                let no_response_since =
                    current_time.saturating_duration_since(*last_connected_time);
                log::info!(
                    "[{}] No response from {} in {:?}",
                    maker.config.port,
                    ip,
                    no_response_since
                );
                if no_response_since > std::time::Duration::from_secs(60) {
                    log::error!(
                        "[{}] Potential Dropped Connection from {}",
                        maker.config.port,
                        ip
                    );
                    // Extract Incoming and Outgoing contracts, and timelock spends of the contract transactions.
                    // fully signed.
                    for (og_sc, ic_sc) in state
                        .outgoing_swapcoins
                        .iter()
                        .zip(state.incoming_swapcoins.iter())
                    {
                        let contract_timelock = og_sc.get_timelock()?;
                        let contract = og_sc.get_fully_signed_contract_tx()?;
                        let next_internal_address =
                            &maker.wallet.read()?.get_next_internal_addresses(1)?[0];
                        let time_lock_spend = og_sc.create_timelock_spend(next_internal_address)?;
                        outgoings.push((
                            (og_sc.get_multisig_redeemscript(), contract),
                            (contract_timelock, time_lock_spend),
                        ));
                        let incoming_contract = ic_sc.get_fully_signed_contract_tx()?;
                        incomings.push((ic_sc.get_multisig_redeemscript(), incoming_contract));
                    }
                    bad_ip.push(*ip);
                    // Spawn a separate thread to wait for contract maturity and broadcasting timelocked.
                    let maker_clone = maker.clone();
                    log::info!(
                        "[{}] Spawning recovery thread after Taker dropped",
                        maker.config.port
                    );
                    let handle = std::thread::Builder::new()
                        .name("Swap Recovery Thread".to_string())
                        .spawn(move || {
                            if let Err(e) = recover_from_swap(maker_clone, outgoings, incomings) {
                                log::error!("Failed to recover from swap due to: {:?}", e);
                            }
                        })?;
                    maker.thread_pool.add_thread(handle);
                    // Clear the state values here
                    *state = ConnectionState::default();
                    break;
                }
            }

            // Clear the state entry here
            for ip in bad_ip.iter() {
                lock_on_state.remove(ip);
            }
        } // All locks are cleared here

        std::thread::sleep(Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
    }

    Ok(())
}

/// Broadcast Incoming and Outgoing Contract transactions & timelock transactions after maturity.
/// Remove contract transactions from the wallet.
pub fn recover_from_swap(
    maker: Arc<Maker>,
    // Tuple of ((Multisig_reedemscript, Contract Tx), (Timelock, Timelock Tx))
    outgoings: Vec<((ScriptBuf, Transaction), (u16, Transaction))>,
    // Tuple of (Multisig Reedemscript, Contract Tx)
    incomings: Vec<(ScriptBuf, Transaction)>,
) -> Result<(), MakerError> {
    // broadcast all the incoming contracts and remove them from the wallet.
    for (incoming_reedemscript, tx) in incomings {
        if maker
            .wallet
            .read()?
            .rpc
            .get_raw_transaction_info(&tx.compute_txid(), None)
            .is_ok()
        {
            log::info!(
                "[{}] Incoming Contract Already Broadcasted",
                maker.config.port
            );
        } else {
            maker
                .wallet
                .read()?
                .rpc
                .send_raw_transaction(&tx)
                .map_err(WalletError::Rpc)?;
            log::info!(
                "[{}] Broadcasted Incoming Contract : {}",
                maker.config.port,
                tx.compute_txid()
            );
        }

        let removed_incoming = maker
            .wallet
            .write()?
            .remove_incoming_swapcoin(&incoming_reedemscript)?
            .expect("Incoming swapcoin expected");
        log::info!(
            "[{}] Removed Incoming Swapcoin From Wallet, Contract Txid : {}",
            maker.config.port,
            removed_incoming.contract_tx.compute_txid()
        );
    }

    maker.wallet.read()?.save_to_disk()?;

    //broadcast all the outgoing contracts
    for ((_, tx), _) in outgoings.iter() {
        if maker
            .wallet
            .read()?
            .rpc
            .get_raw_transaction_info(&tx.compute_txid(), None)
            .is_ok()
        {
            log::info!(
                "[{}] Outgoing Contract already broadcasted",
                maker.config.port
            );
        } else {
            maker
                .wallet
                .read()?
                .rpc
                .send_raw_transaction(tx)
                .map_err(WalletError::Rpc)?;
            log::info!(
                "[{}] Broadcasted Outgoing Contract : {}",
                maker.config.port,
                tx.compute_txid()
            );
        }
    }

    // Check for contract confirmations and broadcast timelocked transaction
    let mut timelock_boardcasted = Vec::new();
    loop {
        for ((_, contract), (timelock, timelocked_tx)) in outgoings.iter() {
            // We have already broadcasted this tx, so skip
            if timelock_boardcasted.contains(&timelocked_tx) {
                continue;
            }
            // Check if the contract tx has reached required maturity
            // Failure here means the transaction hasn't been broadcasted yet. So do nothing and try again.
            if let Ok(result) = maker
                .wallet
                .read()?
                .rpc
                .get_raw_transaction_info(&contract.compute_txid(), None)
            {
                log::info!(
                    "[{}] Contract Tx : {}, reached confirmation : {:?}, Required Confirmation : {}",
                    maker.config.port,
                    contract.compute_txid(),
                    result.confirmations,
                    timelock
                );
                if let Some(confirmation) = result.confirmations {
                    // Now the transaction is confirmed in a block, check for required maturity
                    if confirmation > (*timelock as u32) {
                        log::info!(
                            "[{}] Timelock maturity of {} blocks for Contract Tx is reached : {}",
                            maker.config.port,
                            timelock,
                            contract.compute_txid()
                        );
                        log::info!(
                            "[{}] Broadcasting timelocked tx: {}",
                            maker.config.port,
                            timelocked_tx.compute_txid()
                        );
                        maker
                            .wallet
                            .read()?
                            .rpc
                            .send_raw_transaction(timelocked_tx)
                            .map_err(WalletError::Rpc)?;
                        timelock_boardcasted.push(timelocked_tx);
                    }
                }
            }
        }
        // Everything is broadcasted. Remove swapcoins from wallet
        if timelock_boardcasted.len() == outgoings.len() {
            for ((outgoing_reedemscript, _), _) in outgoings {
                let outgoing_removed = maker
                    .wallet
                    .write()?
                    .remove_outgoing_swapcoin(&outgoing_reedemscript)?
                    .expect("outgoing swapcoin expected");

                log::info!(
                    "[{}] Removed Outgoing Swapcoin from Wallet, Contract Txid: {}",
                    maker.config.port,
                    outgoing_removed.contract_tx.compute_txid()
                );
            }
            log::info!("initializing Wallet Sync.");
            {
                let mut wallet_write = maker.wallet.write()?;
                wallet_write.sync()?;
                wallet_write.save_to_disk()?;
            }
            log::info!("Completed Wallet Sync.");
            // For test, shutdown the maker at this stage.
            #[cfg(feature = "integration-test")]
            maker.shutdown.store(true, Relaxed);
            return Ok(());
        }
        // Sleep before next blockchain scan
        let block_lookup_interval = if cfg!(feature = "integration-test") {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(300)
        };
        std::thread::sleep(block_lookup_interval);
    }
}
