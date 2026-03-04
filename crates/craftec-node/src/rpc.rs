//! [`NodeRpcHandler`] — handles RPC requests from CLI clients and external callers.
//!
//! Implements [`RpcHandler`] to service requests arriving on the `/craftec/rpc/1` ALPN.

use std::sync::Arc;
use std::time::Instant;

use craftec_health::tracker::PieceTracker;
use craftec_net::RpcHandler;
use craftec_net::connection::RpcHandlerFuture;
use craftec_net::swim::SwimMembership;
use craftec_obj::ContentAddressedStore;
use craftec_sql::CraftDatabase;
use craftec_types::identity::{NodeId, verify};
use craftec_types::wire::{RpcRequest, RpcResponse, RpcValue};

/// RPC handler wired to node subsystems.
pub struct NodeRpcHandler {
    node_id: NodeId,
    store: Arc<ContentAddressedStore>,
    database: Arc<CraftDatabase>,
    swim: Arc<SwimMembership>,
    piece_tracker: Arc<PieceTracker>,
    start_time: Instant,
}

impl NodeRpcHandler {
    pub fn new(
        node_id: NodeId,
        store: Arc<ContentAddressedStore>,
        database: Arc<CraftDatabase>,
        swim: Arc<SwimMembership>,
        piece_tracker: Arc<PieceTracker>,
    ) -> Self {
        Self {
            node_id,
            store,
            database,
            swim,
            piece_tracker,
            start_time: Instant::now(),
        }
    }
}

impl RpcHandler for NodeRpcHandler {
    fn handle_request(&self, from: NodeId, req: RpcRequest) -> RpcHandlerFuture {
        let node_id = self.node_id;
        let store = Arc::clone(&self.store);
        let database = Arc::clone(&self.database);
        let swim = Arc::clone(&self.swim);
        let piece_tracker = Arc::clone(&self.piece_tracker);
        let uptime = self.start_time.elapsed().as_secs();

        Box::pin(async move {
            tracing::debug!(peer = %from, req = ?std::mem::discriminant(&req), "RPC request");
            match req {
                RpcRequest::Status => {
                    let store_objects = store.object_count().unwrap_or(0);
                    let db_root_cid = database.root_cid();
                    let alive_peers = swim.alive_count();
                    RpcResponse::Status {
                        node_id,
                        alive_peers,
                        store_objects,
                        db_root_cid,
                        uptime_secs: uptime,
                    }
                }

                RpcRequest::StorePut { data } => match store.put(&data).await {
                    Ok(cid) => RpcResponse::StorePut { cid },
                    Err(e) => RpcResponse::Error {
                        code: 500,
                        message: format!("store put failed: {e}"),
                    },
                },

                RpcRequest::StoreGet { cid } => match store.get(&cid).await {
                    Ok(Some(bytes)) => RpcResponse::StoreGet {
                        data: Some(bytes.to_vec()),
                    },
                    Ok(None) => RpcResponse::StoreGet { data: None },
                    Err(e) => RpcResponse::Error {
                        code: 500,
                        message: format!("store get failed: {e}"),
                    },
                },

                RpcRequest::StoreList => match store.list_cids().await {
                    Ok(cids) => RpcResponse::StoreList { cids },
                    Err(e) => RpcResponse::Error {
                        code: 500,
                        message: format!("store list failed: {e}"),
                    },
                },

                RpcRequest::SqlQuery { sql } => match database.query(&sql).await {
                    Ok(rows) => {
                        let rpc_rows: Vec<Vec<RpcValue>> = rows
                            .into_iter()
                            .map(|row| {
                                row.into_iter()
                                    .map(|col| match col {
                                        craftec_sql::ColumnValue::Null => RpcValue::Null,
                                        craftec_sql::ColumnValue::Integer(v) => {
                                            RpcValue::Integer(v)
                                        }
                                        craftec_sql::ColumnValue::Real(v) => RpcValue::Real(v),
                                        craftec_sql::ColumnValue::Text(v) => RpcValue::Text(v),
                                        craftec_sql::ColumnValue::Blob(v) => RpcValue::Blob(v),
                                    })
                                    .collect()
                            })
                            .collect();
                        RpcResponse::SqlQuery { rows: rpc_rows }
                    }
                    Err(e) => RpcResponse::Error {
                        code: 400,
                        message: format!("query failed: {e}"),
                    },
                },

                RpcRequest::SqlExecute {
                    sql,
                    writer,
                    signature,
                } => {
                    if !verify(sql.as_bytes(), &signature, &writer) {
                        return RpcResponse::Error {
                            code: 403,
                            message: "invalid signature".into(),
                        };
                    }
                    match database.execute(&sql, &writer).await {
                        Ok(()) => RpcResponse::SqlExecute {
                            root_cid: database.root_cid(),
                        },
                        Err(e) => RpcResponse::Error {
                            code: 400,
                            message: format!("execute failed: {e}"),
                        },
                    }
                }

                RpcRequest::Peers => {
                    let peers = swim.alive_members();
                    let count = peers.len();
                    RpcResponse::Peers { peers, count }
                }

                RpcRequest::PieceInfo { cid } => {
                    let available = piece_tracker.available_count(&cid);
                    let k = piece_tracker.get_k(&cid);
                    let holders = piece_tracker.holder_nodes(&cid);
                    RpcResponse::PieceInfo {
                        cid,
                        available,
                        k,
                        holders,
                    }
                }
            }
        })
    }
}
