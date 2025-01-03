use std::{net::TcpStream, time::Duration};
use bitcoin::Amount;


use clap::Parser;
use coinswap::{
    maker::{MakerError, RpcMsgReq, RpcMsgResp},
    utill::{read_message, send_message, setup_maker_logger},
};

/// maker-cli is a command line app to send RPC messages to maker server.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct App {
    /// Sets the rpc-port of Makerd
    #[clap(long, short = 'p', default_value = "127.0.0.1:6103")]
    rpc_port: String,
    /// The command to execute
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    /// Sends a Ping
    Ping,
    /// Returns a list of seed utxos
    SeedUtxo,
    /// Returns a list of swap coin utxos
    SwapUtxo,
    /// Returns a list of live contract utxos
    ContractUtxo,
    /// Returns a list of fidelity utxos
    FidelityUtxo,
    /// Returns the total seed balance
    SeedBalance,
    /// Returns the total swap coin balance
    SwapBalance,
    /// Returns the total live contract balance
    ContractBalance,
    /// Returns the total fidelity balance
    FidelityBalance,
    /// Gets a new address
    NewAddress,
    /// Send to an external address and returns the transaction hex.
    SendToAddress {
        address: String,
        amount: Amount,
        fee: Amount,
    },
    /// Returns the tor address
    GetTorAddress,
    /// Returns the data directory path
    GetDataDir,
    /// Stops the maker server
    Stop,
}

fn main() -> Result<(), MakerError> {
    setup_maker_logger(log::LevelFilter::Info);
    let cli = App::parse();

    let stream = TcpStream::connect(cli.rpc_port)?;

    match cli.command {
        Commands::Ping => {
            send_rpc_req(stream, RpcMsgReq::Ping)?;
        }
        Commands::ContractUtxo => {
            send_rpc_req(stream, RpcMsgReq::ContractUtxo)?;
        }
        Commands::ContractBalance => {
            send_rpc_req(stream, RpcMsgReq::ContractBalance)?;
        }
        Commands::FidelityBalance => {
            send_rpc_req(stream, RpcMsgReq::FidelityBalance)?;
        }
        Commands::FidelityUtxo => {
            send_rpc_req(stream, RpcMsgReq::FidelityUtxo)?;
        }
        Commands::SeedBalance => {
            send_rpc_req(stream, RpcMsgReq::SeedBalance)?;
        }
        Commands::SeedUtxo => {
            send_rpc_req(stream, RpcMsgReq::SeedUtxo)?;
        }
        Commands::SwapBalance => {
            send_rpc_req(stream, RpcMsgReq::SwapBalance)?;
        }
        Commands::SwapUtxo => {
            send_rpc_req(stream, RpcMsgReq::SwapUtxo)?;
        }
        Commands::NewAddress => {
            send_rpc_req(stream, RpcMsgReq::NewAddress)?;
        }
        Commands::SendToAddress {
            address,
            amount,
            fee: Amount,
        } => {
            send_rpc_req(
                stream,
                RpcMsgReq::SendToAddress {
                    address,
                    amount,
                    fee,
                },
            )?;
        }
        Commands::GetTorAddress => {
            send_rpc_req(stream, RpcMsgReq::GetTorAddress)?;
        }
        Commands::GetDataDir => {
            send_rpc_req(stream, RpcMsgReq::GetDataDir)?;
        }
        Commands::Stop => {
            send_rpc_req(stream, RpcMsgReq::Stop)?;
        }
    }

    Ok(())
}

fn send_rpc_req(mut stream: TcpStream, req: RpcMsgReq) -> Result<(), MakerError> {
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;

    send_message(&mut stream, &req)?;

    let response_bytes = read_message(&mut stream)?;
    let response: RpcMsgResp = serde_cbor::from_slice(&response_bytes)?;

    println!("{}", response);

    Ok(())
}
