//! The Coinswap Maker Server.
//!
//! This module includes all server side code for the coinswap maker.
//! The server maintains the thread pool for P2P Connection, Watchtower, Bitcoin Backend and RPC Client Request.
//! The server listens at two port 6102 for P2P, and 6103 for RPC Client request.

use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc,
    },
    thread::{self, sleep},
    time::Duration,
};

#[cfg(feature = "tor")]
use std::io::Read;

#[cfg(feature = "tor")]
use std::{
    fs,
    path::{Path, PathBuf},
};

use bitcoin::{absolute::LockTime, Amount};
use bitcoind::bitcoincore_rpc::RpcApi;

#[cfg(feature = "tor")]
use socks::Socks5Stream;

pub use super::Maker;

use crate::{
    error::NetError,
    maker::{
        api::{check_for_broadcasted_contracts, check_for_idle_states, ConnectionState},
        handlers::handle_message,
        rpc::start_rpc_server,
    },
    protocol::messages::TakerToMakerMessage,
    utill::{read_message, send_message, ConnectionType, DnsMetadata, DnsRequest},
    wallet::WalletError,
};

#[cfg(feature = "tor")]
use crate::utill::monitor_log_for_completion;

use crate::maker::error::MakerError;

// Default values for Maker configurations
pub const HEART_BEAT_INTERVAL_SECS: u64 = 3;
pub const RPC_PING_INTERVAL_SECS: u64 = 60;
pub const _DIRECTORY_SERVERS_REFRESH_INTERVAL_SECS: u64 = 60 * 60 * 12; // 12 Hours
pub const _IDLE_CONNECTION_TIMEOUT: u64 = 300;
pub const REQUIRED_CONFIRMS: BlockHeight = 1;
pub const MIN_CONTRACT_REACTION_TIME: u16 = 48;

/// Fee rate per swap amount in parts per billion (PPB).
/// E.g., for 1 billion sats (0.01 BTC), a value of 10_000 would result in a 0.1% fee.
pub const AMOUNT_RELATIVE_FEE_PPB: Amount = Amount::from_sat(10_000_000);

#[cfg(feature = "tor")]
type OptionalJoinHandle = Option<mitosis::JoinHandle<()>>;

#[cfg(not(feature = "tor"))]
type OptionalJoinHandle = Option<()>;

/// Fetches the Maker and DNS address, and sends maker address to the DNS server.
/// Depending upon ConnectionType and test/prod environment, different maker address and DNS addresses are returned.
/// Return the Maker address and an optional tor thread handle.
///
/// Tor thread is spawned only if ConnectionType=TOR and --feature=tor is enabled.
/// Errors if ConncetionType=TOR but, the tor feature is not enabled.
fn network_bootstrap(maker: Arc<Maker>) -> Result<(String, OptionalJoinHandle), MakerError> {
    let maker_port = maker.config.port;
    let mut tor_handle = None;
    let (maker_address, dns_address) = match maker.config.connection_type {
        ConnectionType::CLEARNET => {
            let maker_address = format!("127.0.0.1:{}", maker_port);
            let dns_address = if cfg!(feature = "integration-test") {
                format!("127.0.0.1:{}", 8080)
            } else {
                maker.config.directory_server_address.clone()
            };

            (maker_address, dns_address)
        }
        #[cfg(feature = "tor")]
        ConnectionType::TOR => {
            let maker_socks_port = maker.config.socks_port;

            let tor_log_dir = format!("/tmp/tor-rust-maker{}/log", maker_port);

            if Path::new(&tor_log_dir).exists() {
                match fs::remove_file(&tor_log_dir) {
                    Ok(_) => log::info!(
                        "[{}] Previous Maker log file deleted successfully",
                        maker_port
                    ),
                    Err(_) => log::error!("[{}] Error deleting Maker log file", maker_port),
                }
            }

            tor_handle = Some(crate::tor::spawn_tor(
                maker_socks_port,
                maker_port,
                format!("/tmp/tor-rust-maker{}", maker_port),
            ));
            thread::sleep(Duration::from_secs(10));

            if let Err(e) = monitor_log_for_completion(&PathBuf::from(tor_log_dir), "100%") {
                log::error!("[{}] Error monitoring log file: {}", maker_port, e);
            }

            log::info!("[{}] Maker tor is instantiated", maker_port);

            let maker_hs_path_str =
                format!("/tmp/tor-rust-maker{}/hs-dir/hostname", maker.config.port);
            let mut maker_file = fs::File::open(maker_hs_path_str)?;
            let mut maker_onion_addr: String = String::new();
            maker_file.read_to_string(&mut maker_onion_addr)?;

            maker_onion_addr.pop(); // Remove `\n` at the end.

            let maker_address = format!("{}:{}", maker_onion_addr, maker.config.port);

            let directory_onion_address = if cfg!(feature = "integration-test") {
                let directory_hs_path_str = "/tmp/tor-rust-directory/hs-dir/hostname";
                let mut directory_file = fs::File::open(directory_hs_path_str)?;
                let mut directory_onion_addr: String = String::new();

                directory_file.read_to_string(&mut directory_onion_addr)?;
                directory_onion_addr.pop(); // Remove `\n` at the end.
                format!("{}:{}", directory_onion_addr, 8080)
            } else {
                maker.config.directory_server_address.clone()
            };

            (maker_address, directory_onion_address)
        }
    };

    setup_fidelity_bond(&maker, &maker_address)?;
    maker.wallet.write()?.refresh_offer_maxsize_cache()?;

    let proof = maker
        .highest_fidelity_proof
        .read()?
        .as_ref()
        .unwrap()
        .clone();

    let dns_metadata = DnsMetadata {
        url: maker_address.clone(),
        proof,
    };

    let request = DnsRequest::Post {
        metadata: Box::new(dns_metadata),
    };

    // Keep trying until send is successful.
    loop {
        let mut stream = match maker.config.connection_type {
            ConnectionType::CLEARNET => match TcpStream::connect(&dns_address) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!(
                        "[{}] TCP connection error with directory, reattempting: {}",
                        maker_port,
                        e
                    );
                    thread::sleep(Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
                    continue;
                }
            },
            #[cfg(feature = "tor")]
            ConnectionType::TOR => {
                match Socks5Stream::connect(
                    format!("127.0.0.1:{}", maker.config.socks_port),
                    dns_address.as_str(),
                ) {
                    Ok(s) => s.into_inner(),
                    Err(e) => {
                        log::warn!(
                            "[{}] TCP connection error with directory, reattempting: {}",
                            maker_port,
                            e
                        );
                        thread::sleep(Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
                        continue;
                    }
                }
            }
        };

        if let Err(e) = send_message(&mut stream, &request) {
            log::warn!(
                "[{}] Failed to send maker address to directory, reattempting: {}",
                maker_port,
                e
            );

            // Wait before reattempting
            std::thread::sleep(std::time::Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
            continue;
        };

        log::info!(
            "[{}] Successfully sent maker address to directory",
            maker_port
        );
        break;
    }

    Ok((maker_address, tor_handle))
}

/// Checks if the wallet already has fidelity bonds. if not, create the first fidelity bond.
fn setup_fidelity_bond(maker: &Arc<Maker>, maker_address: &str) -> Result<(), MakerError> {
    let highest_index = maker.get_wallet().read()?.get_highest_fidelity_index()?;
    if let Some(i) = highest_index {
        let highest_proof = maker
            .get_wallet()
            .read()?
            .generate_fidelity_proof(i, maker_address)?;
        let mut proof = maker.highest_fidelity_proof.write()?;
        *proof = Some(highest_proof);
    } else {
        // No bond in the wallet. Lets attempt to create one.
        let amount = Amount::from_sat(maker.config.fidelity_value);
        let current_height: BlockHeight = maker
            .get_wallet()
            .read()?
            .rpc
            .get_block_count()
            .map_err(WalletError::Rpc)? as u32;

        // Set 950 blocks locktime for test
        let locktime = if cfg!(feature = "integration-test") {
            LockTime::from_height(current_height + 950).map_err(WalletError::Locktime)?
        } else {
            LockTime::from_height(maker.config.fidelity_timelock + current_height)
                .map_err(WalletError::Locktime)?
        };

        let sleep_increment = 10;
        let mut sleep_multiplier = 0;

        log::info!("Fidelity value chosen = {:?} BTC", amount.to_btc());
        log::info!("Fidelity Tx fee = 1000 sats");

        while !maker.shutdown.load(Relaxed) {
            sleep_multiplier += 1;
            // sync the wallet
            maker.get_wallet().write()?.sync()?;

            let fidelity_result = maker
                .get_wallet()
                .write()?
                .create_fidelity(amount, locktime);

            match fidelity_result {
                // Wait for sufficient fund to create fidelity bond.
                // Hard error if fidelity still can't be created.
                Err(e) => {
                    if let WalletError::InsufficientFund {
                        available: Amount,
                        required: Amount,
                    } = e
                    {
                        log::warn!("Insufficient fund to create fidelity bond.");
                        let amount: Amount = required - available;
                        let addr = maker.get_wallet().write()?.get_next_external_address()?;

                        log::info!("Send at least {:.8} BTC to {:?} | If you send extra, that will be added to your swap balance", amount, addr);

                        let total_sleep = sleep_increment * sleep_multiplier.min(10 * 60);
                        log::info!("Next sync in {:?} secs", total_sleep);
                        thread::sleep(Duration::from_secs(total_sleep));
                    } else {
                        log::error!(
                            "[{}] Fidelity Bond Creation failed: {:?}. Shutting Down Maker server",
                            maker.config.port,
                            e
                        );
                        return Err(e.into());
                    }
                }
                Ok(i) => {
                    log::info!("[{}] Successfully created fidelity bond", maker.config.port);
                    let highest_proof = maker
                        .get_wallet()
                        .read()?
                        .generate_fidelity_proof(i, maker_address)?;
                    let mut proof = maker.highest_fidelity_proof.write()?;
                    *proof = Some(highest_proof);

                    // save the wallet data to disk
                    maker.get_wallet().read()?.save_to_disk()?;
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Keep checking if the Bitcoin Core RPC connection is live. Sets the global `accepting_client` flag as per RPC connection status.
///
/// This will not block. Once Core RPC connection is live, accepting_client will set as `true` again.
fn check_connection_with_core(
    maker: Arc<Maker>,
    accepting_clients: Arc<AtomicBool>,
) -> Result<(), MakerError> {
    let mut rpc_ping_success = false;
    while !maker.shutdown.load(Relaxed) {
        // If connection is disrupted keep trying at heart_beat_interval (3 sec).
        // If connection is live, keep tring at rpc_ping_interval (60 sec).
        match rpc_ping_success {
            true => {
                sleep(Duration::from_secs(RPC_PING_INTERVAL_SECS));
            }
            false => {
                sleep(Duration::from_secs(HEART_BEAT_INTERVAL_SECS));
            }
        }
        if let Err(e) = maker.wallet.read()?.rpc.get_blockchain_info() {
            log::info!(
                "[{}] RPC Connection failed. Reattempting {}",
                maker.config.port,
                e
            );
            rpc_ping_success = false;
        } else {
            rpc_ping_success = true;
        }
        accepting_clients.store(rpc_ping_success, Relaxed);
    }

    Ok(())
}

/// Handle a single client connection.
fn handle_client(
    maker: Arc<Maker>,
    stream: &mut TcpStream,
    client_addr: SocketAddr,
) -> Result<(), MakerError> {
    stream.set_nonblocking(false)?; // Block this thread until message is read.

    let mut connection_state = ConnectionState::default();

    while !maker.shutdown.load(Relaxed) {
        let mut taker_msg_bytes = Vec::new();
        match read_message(stream) {
            Ok(b) => taker_msg_bytes = b,
            Err(e) => {
                if let NetError::IO(e) = e {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        continue;
                    } else {
                        // For any other errors, report them
                        log::error!("[{}] Net Error: {}", maker.config.port, e);
                        continue;
                    }
                }
            }
        }

        let taker_msg: TakerToMakerMessage = serde_cbor::from_slice(&taker_msg_bytes)?;
        log::info!("[{}]  <=== {}", maker.config.port, taker_msg);

        let reply = handle_message(&maker, &mut connection_state, taker_msg, client_addr.ip());

        match reply {
            Ok(reply) => {
                if let Some(message) = reply {
                    log::info!("[{}] ===> {} ", maker.config.port, message);
                    if let Err(e) = send_message(stream, &message) {
                        log::error!("Closing due to IO error in sending message: {:?}", e);
                        continue;
                    }
                } else {
                    continue;
                }
            }
            Err(err) => {
                match &err {
                    // Shutdown server if special behavior is set
                    MakerError::SpecialBehaviour(sp) => {
                        log::error!("[{}] Maker Special Behavior : {:?}", maker.config.port, sp);
                        maker.shutdown.store(true, Relaxed);
                    }
                    e => {
                        log::error!(
                            "[{}] Internal message handling error occurred: {:?}",
                            maker.config.port,
                            e
                        );
                    }
                }
                return Err(err);
            }
        }
    }

    Ok(())
}

// The main Maker Server process.
pub fn start_maker_server(maker: Arc<Maker>) -> Result<(), MakerError> {
    log::info!("Starting Maker Server");
    // Initialize network connections.

    // Setup the wallet with fidelity bond.
    let port = maker.config.port;
    let network = maker.get_wallet().read()?.store.network;
    let balance = maker.get_wallet().read()?.balance()?;
    log::info!("[{}] Currency Network: {:?}", port, network);
    log::info!("[{}] Total Wallet Balance: {:?}", port, balance);

    let (maker_address, tor_thread) = network_bootstrap(maker.clone())?;

    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, maker.config.port)).map_err(NetError::IO)?;
    log::info!(
        "[{}] Listening for client conns at: {}",
        maker.config.port,
        listener.local_addr()?
    );
    listener.set_nonblocking(true)?; // Needed to not block a thread waiting for incoming connection.
    log::info!(
        "[{}] Maker Server Address: {}",
        maker.config.port,
        maker_address
    );

    let heart_beat_interval = HEART_BEAT_INTERVAL_SECS; // All maker internal threads loops at this frequency.

    // Global server Mutex, to switch on/off p2p network.
    let accepting_clients = Arc::new(AtomicBool::new(false));

    if !maker.shutdown.load(Relaxed) {
        // 1. Bitcoin Core Connection checker thread.
        // Ensures that Bitcoin Core connection is live.
        // If not, it will block p2p connections until Core works again.
        let maker_clone = maker.clone();
        let acc_client_clone = accepting_clients.clone();
        let conn_check_thread = thread::Builder::new()
            .name("Bitcoin Core Connection Checker Thread".to_string())
            .spawn(move || {
                log::info!("[{}] Spawning Bitcoin Core connection checker thread", port);
                if let Err(e) = check_connection_with_core(maker_clone.clone(), acc_client_clone) {
                    log::error!("[{}] Bitcoin Core connection check failed: {:?}", port, e);
                    maker_clone.shutdown.store(true, Relaxed);
                }
            })?;
        maker.thread_pool.add_thread(conn_check_thread);

        // 2. Idle Client connection checker thread.
        // This threads check idelness of peer in live swaps.
        // And takes recovery measure if the peer seems to have disappeared in middlle of a swap.
        let maker_clone = maker.clone();
        let idle_conn_check_thread = thread::Builder::new()
            .name("Idle Client Checker Thread".to_string())
            .spawn(move || {
                log::info!(
                    "[{}] Spawning Client connection status checker thread",
                    port
                );
                if let Err(e) = check_for_idle_states(maker_clone.clone()) {
                    log::error!("Failed checking client's idle state {:?}", e);
                    maker_clone.shutdown.store(true, Relaxed);
                }
            })?;
        maker.thread_pool.add_thread(idle_conn_check_thread);

        // 3. Watchtower thread.
        // This thread checks for broadcasted contract transactions, which usually means violation of the protocol.
        // When contract transaction detected in mempool it will attempt recovery.
        // This can get triggered even when contracts of adjacent hops are published. Implying the whole swap route is disrupted.
        let maker_clone = maker.clone();
        let contract_watcher_thread = thread::Builder::new()
            .name("Contract Watcher Thread".to_string())
            .spawn(move || {
                log::info!("[{}] Spawning contract-watcher thread", port);
                if let Err(e) = check_for_broadcasted_contracts(maker_clone.clone()) {
                    maker_clone.shutdown.store(true, Relaxed);
                    log::error!("Failed checking broadcasted contracts {:?}", e);
                }
            })?;
        maker.thread_pool.add_thread(contract_watcher_thread);

        // 4: The RPC server thread.
        // User for responding back to `maker-cli` apps.
        let maker_clone = maker.clone();
        let rpc_thread = thread::Builder::new()
            .name("RPC Thread".to_string())
            .spawn(move || {
                log::info!("[{}] Spawning RPC server thread", port);
                match start_rpc_server(maker_clone.clone()) {
                    Ok(_) => (),
                    Err(e) => {
                        log::error!("Failed starting rpc server {:?}", e);
                        maker_clone.shutdown.store(true, Relaxed);
                    }
                }
            })?;

        maker.thread_pool.add_thread(rpc_thread);

        sleep(Duration::from_secs(heart_beat_interval)); // wait for 1 beat, to complete spawns of all the threads.
        maker.is_setup_complete.store(true, Relaxed);
        log::info!("[{}] Maker setup is ready", maker.config.port);
    }

    // The P2P Client connection loop.
    // Each client connection will spawn a new handler thread, which is added back in the global thread_pool.
    // This loop beats at `maker.config.heart_beat_interval_secs`
    while !maker.shutdown.load(Relaxed) {
        let maker = maker.clone(); // This clone is needed to avoid moving the Arc<Maker> in each iterations.
        let heart_beat_interval = HEART_BEAT_INTERVAL_SECS;

        // Block client connections if accepting_client=false
        if !accepting_clients.load(Relaxed) {
            log::debug!(
                "[{}] Temporary failure in backend node. Not accepting swap request. Check your node if this error persists",
                maker.config.port
            );
            sleep(Duration::from_secs(heart_beat_interval));
            continue;
        }

        match listener.accept() {
            Ok((mut stream, client_addr)) => {
                log::info!("[{}] Spawning Client Handler thread", maker.config.port);
                let maker_for_handler = maker.clone();
                let client_handler_thread = thread::Builder::new()
                    .name("Client Handler Thread".to_string())
                    .spawn(move || {
                        if let Err(e) = handle_client(maker_for_handler, &mut stream, client_addr) {
                            log::error!("[{}] Error Handling client request {:?}", port, e);
                        }
                    })?;
                maker.thread_pool.add_thread(client_handler_thread);
            }

            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    // Do nothing
                } else {
                    log::error!(
                        "[{}] Error accepting incoming connection: {:?}",
                        maker.config.port,
                        e
                    );
                    return Err(NetError::IO(e).into());
                }
            }
        };

        sleep(Duration::from_secs(heart_beat_interval));
    }

    log::info!("[{}] Maker is shutting down.", port);
    #[cfg(feature = "tor")]
    {
        if maker.config.connection_type == ConnectionType::TOR && cfg!(feature = "tor") {
            crate::tor::kill_tor_handles(tor_thread.expect("Tor thread expected"));
        }
    }
    log::info!("Shutdown wallet sync initiated.");
    maker.get_wallet().write()?.sync()?;
    log::info!("Shutdown wallet syncing completed.");
    maker.get_wallet().read()?.save_to_disk()?;
    log::info!("Wallet file saved to disk.");
    log::info!("Maker Server is shut down successfully");
    Ok(())
}
