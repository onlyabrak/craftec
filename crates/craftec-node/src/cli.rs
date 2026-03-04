//! CLI client — subcommand dispatch for `craftec status`, `craftec store put`, etc.
//!
//! Connects to the local node via the `/craftec/rpc/1` ALPN and issues RPC requests
//! over bidi QUIC streams.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use craftec_types::cid::Cid;
use craftec_types::identity::NodeKeypair;
use craftec_types::wire::{RpcRequest, RpcResponse, decode_rpc_response, encode_rpc_request};

/// Run the CLI client, dispatching to the appropriate RPC subcommand.
///
/// Returns `Ok(())` on success, exits the process on error.
pub async fn run_cli(args: &[String]) -> Result<()> {
    let data_dir = data_dir()?;
    let listen_port = listen_port();
    let node_id_bytes = read_node_id(&data_dir)?;
    let addr: SocketAddr = format!("127.0.0.1:{listen_port}").parse()?;

    let request = match args.first().map(|s| s.as_str()) {
        Some("status") => RpcRequest::Status,

        Some("store") => match args.get(1).map(|s| s.as_str()) {
            Some("put") => {
                let mut data = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut data)
                    .context("failed to read stdin")?;
                RpcRequest::StorePut { data }
            }
            Some("get") => {
                let cid_hex = args.get(2).context("usage: craftec store get <cid>")?;
                let cid =
                    Cid::from_str(cid_hex).map_err(|e| anyhow::anyhow!("invalid CID: {e}"))?;
                RpcRequest::StoreGet { cid }
            }
            Some("list") => RpcRequest::StoreList,
            _ => bail!("usage: craftec store <put|get|list>"),
        },

        Some("query") => {
            let sql = args.get(1).context("usage: craftec query \"<sql>\"")?;
            RpcRequest::SqlQuery { sql: sql.clone() }
        }

        Some("execute") => {
            let sql = args.get(1).context("usage: craftec execute \"<sql>\"")?;
            let keypair = load_keypair(&data_dir)?;
            let signature = keypair.sign(sql.as_bytes());
            let writer = keypair.node_id();
            RpcRequest::SqlExecute {
                sql: sql.clone(),
                writer,
                signature,
            }
        }

        Some("peers") => RpcRequest::Peers,

        Some("pieces") => {
            let cid_hex = args.get(1).context("usage: craftec pieces <cid>")?;
            let cid = Cid::from_str(cid_hex).map_err(|e| anyhow::anyhow!("invalid CID: {e}"))?;
            RpcRequest::PieceInfo { cid }
        }

        _ => bail!(
            "usage: craftec <status|store|query|execute|peers|pieces>\n\
             subcommands:\n  \
             status              Node status (JSON)\n  \
             store put           Read stdin, store, print CID\n  \
             store get <cid>     Retrieve data, write to stdout\n  \
             store list          List stored CIDs\n  \
             query \"<sql>\"       SQL query (JSON rows)\n  \
             execute \"<sql>\"     SQL execute (print root CID)\n  \
             peers               Peer list (JSON)\n  \
             pieces <cid>        Piece info (JSON)"
        ),
    };

    let response = send_rpc(&node_id_bytes, addr, request).await?;
    print_response(response, args.first().map(|s| s.as_str()).unwrap_or(""));
    Ok(())
}

/// Overall timeout for a CLI RPC request (connect + send + receive).
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Send an RPC request and receive the response.
///
/// The entire RPC flow (connect, send, receive) is bounded by a 10-second timeout.
/// Response payloads are limited to 4 MiB.
async fn send_rpc(
    node_id_bytes: &[u8; 32],
    addr: SocketAddr,
    request: RpcRequest,
) -> Result<RpcResponse> {
    tokio::time::timeout(RPC_TIMEOUT, send_rpc_inner(node_id_bytes, addr, request))
        .await
        .map_err(|_| anyhow::anyhow!("RPC request timed out after {}s", RPC_TIMEOUT.as_secs()))?
}

async fn send_rpc_inner(
    node_id_bytes: &[u8; 32],
    addr: SocketAddr,
    request: RpcRequest,
) -> Result<RpcResponse> {
    let endpoint = craftec_net::create_rpc_client_endpoint()
        .await
        .context("failed to create RPC client endpoint")?;
    let conn = craftec_net::rpc_connect(&endpoint, node_id_bytes, addr)
        .await
        .context("failed to connect to local node")?;

    let (mut send, mut recv) = conn.open_bi().await.context("failed to open bidi stream")?;

    let req_bytes = encode_rpc_request(&request, 0)?;
    send.write_all(&req_bytes).await?;
    send.finish()?;

    let resp_bytes = recv
        .read_to_end(4 * 1024 * 1024)
        .await
        .context("failed to read RPC response")?;
    let (resp, _hlc_ts) = decode_rpc_response(&resp_bytes)?;
    endpoint.close().await;
    Ok(resp)
}

/// Print the RPC response to stdout.
fn print_response(resp: RpcResponse, cmd: &str) {
    match resp {
        RpcResponse::Status {
            node_id,
            alive_peers,
            store_objects,
            db_root_cid,
            uptime_secs,
        } => {
            let json = serde_json::json!({
                "node_id": node_id.to_string(),
                "alive_peers": alive_peers,
                "store_objects": store_objects,
                "db_root_cid": db_root_cid.to_string(),
                "uptime_secs": uptime_secs,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }

        RpcResponse::StorePut { cid } => {
            println!("{cid}");
        }

        RpcResponse::StoreGet { data } => {
            if let Some(bytes) = data {
                std::io::stdout().write_all(&bytes).unwrap();
            } else {
                eprintln!("not found");
                std::process::exit(1);
            }
        }

        RpcResponse::StoreList { cids } => {
            for cid in &cids {
                println!("{cid}");
            }
        }

        RpcResponse::SqlQuery { rows } => {
            let json_rows: Vec<Vec<serde_json::Value>> = rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|v| match v {
                            craftec_types::RpcValue::Null => serde_json::Value::Null,
                            craftec_types::RpcValue::Integer(i) => {
                                serde_json::Value::Number(i.into())
                            }
                            craftec_types::RpcValue::Real(f) => serde_json::json!(f),
                            craftec_types::RpcValue::Text(s) => serde_json::Value::String(s),
                            craftec_types::RpcValue::Blob(b) => {
                                serde_json::Value::String(hex::encode(b))
                            }
                        })
                        .collect()
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_rows).unwrap());
        }

        RpcResponse::SqlExecute { root_cid } => {
            println!("{root_cid}");
        }

        RpcResponse::Peers { peers, count } => {
            let json = serde_json::json!({
                "count": count,
                "peers": peers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }

        RpcResponse::PieceInfo {
            cid,
            available,
            k,
            holders,
        } => {
            let json = serde_json::json!({
                "cid": cid.to_string(),
                "available": available,
                "k": k,
                "holders": holders.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }

        RpcResponse::Error { code, message } => {
            eprintln!("error {code}: {message}");
            std::process::exit(1);
        }
    }

    // Suppress unused-variable warning for cmd.
    let _ = cmd;
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn data_dir() -> Result<PathBuf> {
    // 1. Explicit env var (Docker sets this).
    if let Ok(dir) = std::env::var("CRAFTEC_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    // 2. Read from craftec.json in the current directory (matches main.rs config loading).
    if let Ok(contents) = std::fs::read_to_string("craftec.json")
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(dir) = json.get("data_dir").and_then(|v| v.as_str())
    {
        return Ok(PathBuf::from(dir));
    }
    // 3. Default to ./data for local dev.
    Ok(PathBuf::from("./data"))
}

fn listen_port() -> u16 {
    std::env::var("CRAFTEC_LISTEN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4433)
}

fn read_node_id(data_dir: &std::path::Path) -> Result<[u8; 32]> {
    let path = data_dir.join("node.id");
    let hex_str = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read node.id at {}", path.display()))?;
    let bytes = hex::decode(hex_str.trim()).with_context(|| "invalid hex in node.id")?;
    if bytes.len() != 32 {
        bail!(
            "node.id must contain 32 bytes (64 hex chars), got {}",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn load_keypair(data_dir: &std::path::Path) -> Result<NodeKeypair> {
    let key_path = data_dir.join("node.key");
    let bytes = std::fs::read(&key_path)
        .with_context(|| format!("failed to read node.key at {}", key_path.display()))?;
    if bytes.len() != 32 {
        bail!("node.key must be 32 bytes, got {}", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(NodeKeypair::from_secret_bytes(&arr))
}
