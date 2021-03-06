use axum::{extract::Extension, http::StatusCode, routing::post, AddExtensionLayer, Json, Router};
use bb8::Pool;
use clap::Parser;
use hbase_thrift::{
    hbase::HbaseSyncClient, BatchMutationBuilder, MutationBuilder, THbaseSyncClientExt,
};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use thrift::{
    protocol::{TBinaryInputProtocol, TBinaryOutputProtocol},
    transport::{
        ReadHalf, TBufferedReadTransport, TBufferedWriteTransport, TTcpChannel, WriteHalf,
    },
};
use thrift_pool::{MakeThriftConnectionFromAddrs, ThriftConnectionManager};
use tower_http::trace::TraceLayer;

type Client = HbaseSyncClient<
    TBinaryInputProtocol<TBufferedReadTransport<ReadHalf<TTcpChannel>>>,
    TBinaryOutputProtocol<TBufferedWriteTransport<WriteHalf<TTcpChannel>>>,
>;
type ConnectionManager<S> = ThriftConnectionManager<MakeThriftConnectionFromAddrs<Client, S>>;
type ConnectionPool<S> = Pool<ConnectionManager<S>>;

type Logs = Vec<BTreeMap<String, Box<RawValue>>>;

#[derive(Debug, Clone)]
struct Config {
    pub column_family: String,
    pub table_name: String,
}

#[derive(Parser)]
#[clap(version, about, author)]
struct Cli {
    /// Address where hbase's thrift endpoint is exposed
    #[clap(long, default_value = "localhost:9090", env)]
    pub hbase_addr: String,

    /// Name of the table in hbase where logs will be written
    #[clap(long, default_value = "logs", env)]
    pub table_name: String,

    /// Name of the column family where logs will be written
    #[clap(long, default_value = "data", env)]
    pub column_family: String,

    /// The path where the endpoint will be enabled
    #[clap(long, default_value = "/", env)]
    pub listen_route: String,

    /// Socket address on which to start the server (address:port)
    #[clap(long, default_value = "0.0.0.0:3000", env)]
    pub listen_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let manager =
        MakeThriftConnectionFromAddrs::<Client, _>::new(cli.hbase_addr).into_connection_manager();
    let pool = Pool::builder()
        .connection_timeout(Duration::from_secs(5))
        .build(manager)
        .await?;

    let app = Router::new()
        .route("/", post(put_logs))
        .layer(AddExtensionLayer::new(pool))
        .layer(AddExtensionLayer::new(Config {
            column_family: cli.column_family,
            table_name: cli.table_name,
        }))
        .layer(TraceLayer::new_for_http());

    tracing::debug!("listening on {}", cli.listen_addr);
    axum::Server::bind(&cli.listen_addr)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

async fn put_logs<'a>(
    Json(logs): Json<Logs>,
    Extension(pool): Extension<ConnectionPool<String>>,
    Extension(config): Extension<Config>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut conn = pool.get().await.map_err(internal_error)?;
    let mut row_batches = Vec::new();
    for log in logs {
        let mut bmb = <BatchMutationBuilder>::default();
        for (k, v) in log {
            let mut mb = MutationBuilder::default();
            mb.value(v.get());
            mb.column(config.column_family.clone(), k);
            bmb.mutation(mb);
        }
        row_batches.push(bmb.build());
    }
    conn.put(&config.table_name, row_batches, None, None)
        .map_err(internal_error)?;
    Ok(StatusCode::CREATED)
}

fn internal_error<E>(err: E) -> (StatusCode, String)
where
    E: std::error::Error,
{
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}
