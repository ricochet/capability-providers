//! # wasmCloud sqldb-postgres capability provider
//!
//! Enables actors to access postgres back-end database through the
//! 'wasmcloud:sqldb' capability.
//!

use bb8_postgres::tokio_postgres::NoTls;
#[allow(unused_imports)]
use log::{debug, error, info, trace};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use tokio::sync::RwLock;
use wasmbus_rpc::provider::prelude::*;
use wasmcloud_interface_sqldb::{Column, ExecuteResult, FetchResult, Query, SqlDb, SqlDbReceiver};

mod config;
mod error;
use error::DbError;

mod types;

// main (via provider_main) initializes the threaded tokio executor,
// listens to lattice rpcs, handles actor links,
// and returns only when it receives a shutdown message
//
fn main() -> Result<(), Box<dyn std::error::Error>> {
    provider_main(SqlDbProvider::default())?;

    eprintln!("sqldb provider exiting");
    Ok(())
}

//pub(crate) type NTLS = native_tls::TlsConnector;
pub(crate) type PgConnection = bb8_postgres::PostgresConnectionManager<NoTls>;
pub(crate) type Pool = bb8_postgres::bb8::Pool<PgConnection>;

/// sqldb capability provider implementation
#[derive(Default, Clone, Provider)]
#[services(SqlDb)]
struct SqlDbProvider {
    actors: Arc<RwLock<HashMap<String, Pool>>>,
}

/// use default implementations of provider message handlers
impl ProviderDispatch for SqlDbProvider {}

/// Handle connection pools for each link
#[async_trait]
impl ProviderHandler for SqlDbProvider {
    /// Provider should perform any operations needed for a new link,
    /// including setting up per-actor resources, and checking authorization.
    /// If the link is allowed, return true, otherwise return false to deny the link.
    async fn put_link(&self, ld: &LinkDefinition) -> RpcResult<bool> {
        let config = config::load_config(ld)?;
        let pool = config::create_pool(config).await?;
        let mut update_map = self.actors.write().await;
        update_map.insert(ld.actor_id.to_string(), pool);
        Ok(true)
    }

    /// Handle notification that a link is dropped - close the connection
    async fn delete_link(&self, actor_id: &str) {
        let mut aw = self.actors.write().await;
        if let Some(conn) = aw.remove(actor_id) {
            // close all connections for this actor-link's pool
            drop(conn);
        }
    }

    /// Handle shutdown request by closing all connections
    async fn shutdown(&self) -> Result<(), Infallible> {
        let mut aw = self.actors.write().await;
        // close all connections
        for (_, conn) in aw.drain() {
            drop(conn);
        }
        Ok(())
    }
}

fn actor_id(ctx: &Context) -> Result<&String, RpcError> {
    ctx.actor
        .as_ref()
        .ok_or_else(|| RpcError::InvalidParameter("no actor in request".into()))
}

/// SqlDb - SQL Database connections
/// To use this capability, the actor must be linked
/// with "wasmcloud:sqldb"
/// wasmbus.contractId: wasmcloud:sqldb
/// wasmbus.providerReceive
#[async_trait]
impl SqlDb for SqlDbProvider {
    async fn execute(&self, ctx: &Context, query: &Query) -> RpcResult<ExecuteResult> {
        let actor_id = actor_id(ctx)?;
        let rd = self.actors.read().await;
        let pool = rd
            .get(actor_id)
            .ok_or_else(|| RpcError::InvalidParameter(format!("actor not linked:{}", actor_id)))?;
        let conn = match pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                return Ok(ExecuteResult {
                    error: Some(DbError::Io(format!("connection pool: {}", e)).into()),
                    ..Default::default()
                })
            }
        };
        match conn.execute(query.as_str(), &[]).await {
            Ok(res) => Ok(ExecuteResult {
                rows_affected: res,
                ..Default::default()
            }),
            Err(db_err) => {
                error!(
                    "{} query:'{}' error:{}",
                    actor_id,
                    query,
                    &db_err.to_string()
                );
                Ok(ExecuteResult {
                    error: Some(DbError::from(db_err).into()),
                    ..Default::default()
                })
            }
        }
    }

    /// perform select query on database, returning all result rows
    async fn fetch(&self, ctx: &Context, query: &Query) -> RpcResult<FetchResult> {
        let actor_id = actor_id(ctx)?;
        let rd = self.actors.read().await;
        let pool = rd
            .get(actor_id)
            .ok_or_else(|| RpcError::InvalidParameter(format!("actor not linked:{}", actor_id)))?;
        let conn = match pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                return Ok(FetchResult {
                    error: Some(DbError::Io(format!("connection pool: {}", e)).into()),
                    ..Default::default()
                });
            }
        };

        match conn.query(query.as_str(), &[]).await {
            Ok(rows) => {
                if rows.is_empty() {
                    Ok(FetchResult::default())
                } else {
                    let cols = rows
                        .get(0)
                        .unwrap()
                        .columns()
                        .iter()
                        .enumerate()
                        .map(|(i, c)| Column {
                            name: c.name().to_string(),
                            ordinal: i as u32,
                            db_type: c.type_().name().to_string(),
                        })
                        .collect::<Vec<Column>>();
                    match encode_result_set(&rows) {
                        Ok(buf) => Ok(FetchResult {
                            columns: cols,
                            num_rows: rows.len() as u64,
                            error: None,
                            rows: buf,
                        }),
                        Err(e) => Ok(FetchResult {
                            error: Some(e.into()),
                            ..Default::default()
                        }),
                    }
                }
            }
            Err(db_err) => {
                error!(
                    "{} query:'{}' error:{}",
                    actor_id,
                    query,
                    &db_err.to_string()
                );
                Ok(FetchResult {
                    error: Some(DbError::from(db_err).into()),
                    ..Default::default()
                })
            }
        }
    }
}

fn encode_result_set(rows: &[tokio_postgres::Row]) -> Result<Vec<u8>, DbError> {
    let mut buf = Vec::with_capacity(rows.len() * 2);
    let mut enc = minicbor::Encoder::new(&mut buf);
    types::encode_rows(&mut enc, rows).map_err(|e| DbError::Encoding(e.to_string()))?;
    Ok(buf)
}
