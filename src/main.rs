mod alchemy_client;
mod indexer_selection;
mod prelude;
mod query_engine;
mod sync_client;
mod ws_client;

use crate::{indexer_selection::SecretKey, prelude::*, query_engine::*};
use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse},
    http::{header, HeaderName, StatusCode},
    web, App, HttpRequest, HttpResponse, HttpResponseBuilder, HttpServer,
};
use async_trait::async_trait;
use hex;
use indexer_selection::{IndexerQuery, UnresolvedBlock, UtilityConfig};
use lazy_static::lazy_static;
use prometheus::{self, Encoder as _};
use reqwest;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::{
    collections::HashMap,
    error::Error,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering as MemoryOrdering},
        Arc,
    },
};
use structopt::StructOpt;
use structopt_derive::StructOpt;
use tokio::time::Duration;

#[derive(StructOpt, Debug)]
struct Opt {
    #[structopt(
        help = "URL of gateway agent syncing API",
        long = "--sync-agent",
        env = "SYNC_AGENT"
    )]
    sync_agent: String,
    #[structopt(
        help = "Ethereum provider URLs, format: '<network>=<url>,...'\ne.g. rinkeby=eth-rinkeby.alchemyapi.io/v2/<api-key>",
        long = "--ethereum-providers",
        env = "ETHEREUM_PROVIDERS",
        parse(try_from_str = "parse_networks")
    )]
    ethereum_proviers: Vec<(String, String)>,
    #[structopt(
        help = "Network subgraph URL",
        long = "--network-subgraph",
        env = "NETWORK_SUBGRAPH"
    )]
    network_subgraph: String,
    #[structopt(help = "Format log output as JSON", long = "--log-json")]
    log_json: bool,
    #[structopt(
        long = "--indexer-selection-retry-limit",
        env = "INDEXER_SELECTION_LIMIT",
        default_value = "5"
    )]
    indexer_selection_retry_limit: usize,
    #[structopt(
        long = "--query-budget",
        env = "QUERY_BUDGET",
        default_value = "0.0005"
    )]
    query_budget: GRT,
    #[structopt(long = "--port", env = "PORT", default_value = "6700")]
    port: u16,
    #[structopt(long = "--metrics-port", env = "METRICS_PORT", default_value = "7300")]
    metrics_port: u16,
}

fn parse_networks(arg: &str) -> Result<(String, String), String> {
    let kv = arg.split("=").collect::<Vec<&str>>();
    if kv.len() != 2 {
        return Err("networks syntax: <network>=<url>,...".into());
    }
    Ok((kv[0].into(), kv[1].into()))
}

#[actix_web::main]
async fn main() {
    let opt = Opt::from_args();
    init_tracing(opt.log_json);
    tracing::info!("Graph gateway starting...");
    tracing::trace!("{:#?}", opt);

    // TODO: set from mnemonic env var
    let signer_key =
        SecretKey::from_str("244226452948404D635166546A576E5A7234753778217A25432A462D4A614E64")
            .expect("Invalid mnemonic");

    let (input_writers, inputs) = Inputs::new();
    let (block_resolvers, block_metrics): (
        HashMap<String, mpsc::Sender<alchemy_client::Request>>,
        Vec<alchemy_client::Metrics>,
    ) = opt
        .ethereum_proviers
        .into_iter()
        .map(|(network, ws_url)| {
            let (send, metrics) =
                alchemy_client::create(network.clone(), ws_url, input_writers.indexers.clone());
            ((network, send), metrics)
        })
        .unzip();
    let sync_metrics = sync_client::create(
        opt.sync_agent,
        Duration::from_secs(30),
        signer_key.clone(),
        input_writers,
    );
    let config = query_engine::Config {
        indexer_selection_retry_limit: opt.indexer_selection_retry_limit,
        utility: UtilityConfig::default(),
        query_budget: opt.query_budget,
    };
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    // TODO: argument for timeout
    let resolver = NetworkResolver {
        block_resolvers: Arc::new(block_resolvers),
        client: http_client.clone(),
    };
    static QUERY_ID: AtomicUsize = AtomicUsize::new(0);
    let network_subgraph = opt.network_subgraph;
    let metrics_port = opt.metrics_port;
    // Host metrics on a separate server with a port that isn't open to public requests.
    actix_web::rt::spawn(async move {
        HttpServer::new(move || App::new().route("/metrics", web::get().to(handle_metrics)))
            .workers(1)
            .bind(("0.0.0.0", metrics_port))
            .expect("Failed to bind to metrics port")
            .run()
            .await
            .expect("Failed to start metrics server");
    });
    // TODO: rate limit API keys
    // TODO: rate limit without API keys
    HttpServer::new(move || {
        let api = web::scope("/api/{api_key}")
            .app_data(web::Data::new((
                config.clone(),
                resolver.clone(),
                inputs.clone(),
                &QUERY_ID,
            )))
            .route(
                "/subgraphs/id/{subgraph_id}",
                web::post().to(handle_subgraph_query),
            )
            .route(
                "/deployments/id/{deployment_id}",
                web::post().to(handle_subgraph_query),
            );
        App::new()
            .wrap_fn(reject_bad_headers)
            .service(api)
            .route("/", web::get().to(|| async { "Ready to roll!" }))
            .service(
                web::resource("/ready")
                    .app_data(web::Data::new((
                        block_metrics.clone(),
                        sync_metrics.clone(),
                    )))
                    .route(web::get().to(handle_ready)),
            )
            .service(
                web::resource("/network")
                    .app_data(web::Data::new((
                        http_client.clone(),
                        network_subgraph.clone(),
                    )))
                    .route(web::post().to(handle_network_query)),
            )
            .service(
                web::resource("/collect-receipts")
                    .app_data(web::PayloadConfig::new(16_000_000))
                    .app_data(web::Data::new(signer_key.clone()))
                    .route(web::post().to(handle_collect_receipts)),
            )
    })
    .bind(("0.0.0.0", opt.port))
    .expect("Failed to bind")
    .run()
    .await
    .expect("Failed to start server");
}

#[tracing::instrument]
async fn handle_metrics() -> HttpResponse {
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    if let Err(metrics_encode_err) = encoder.encode(&metric_families, &mut buffer) {
        tracing::error!(%metrics_encode_err);
        return HttpResponseBuilder::new(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Failed to encode metrics");
    }
    HttpResponseBuilder::new(StatusCode::OK).body(buffer)
}

fn reject_bad_headers<S>(
    mut request: ServiceRequest,
    service: &S,
) -> impl Future<Output = Result<ServiceResponse, actix_web::Error>>
where
    S: Service<ServiceRequest, Response = ServiceResponse, Error = actix_web::Error>,
{
    lazy_static! {
        static ref BAD_HEADERS: [HeaderName; 1] =
            [HeaderName::from_lowercase(b"challenge-bypass-token").unwrap()];
    }
    let contains_bad_header = BAD_HEADERS
        .iter()
        .any(|header| request.headers().contains_key(header));
    // This mess is necessary since some side-effect of cloning the HTTP Request part of the
    // ServiceRequest will result in a panic in actix-web if the service is called. An enum would be
    // better, but the types involved cannot be expressed.
    let (result, err) = if !contains_bad_header {
        (Some(service.call(request)), None)
    } else {
        let http_req = request.parts_mut().0.clone();
        let err = ServiceResponse::new(http_req, HttpResponse::BadRequest().finish());
        (None, Some(err))
    };
    async move {
        match result {
            Some(result) => result.await,
            None => Ok(err.unwrap()),
        }
    }
}

#[tracing::instrument(skip(data))]
async fn handle_ready(
    data: web::Data<(Vec<alchemy_client::Metrics>, sync_client::Metrics)>,
) -> HttpResponse {
    let ready = data.0.iter().all(|metrics| metrics.head_block.get() > 0)
        && (data.1.allocations.get() > 0)
        && (data.1.transfers.get() > 0);
    if ready {
        HttpResponseBuilder::new(StatusCode::OK).body("Ready")
    } else {
        // Respond with 425 Too Early
        HttpResponseBuilder::new(StatusCode::from_u16(425).unwrap()).body("Not ready")
    }
}

#[tracing::instrument(skip(payload))]
async fn handle_collect_receipts(data: web::Data<SecretKey>, payload: String) -> HttpResponse {
    let _timer = METRICS.collect_receipts_duration.start_timer();
    let bytes = payload.into_bytes();
    if bytes.len() < 20 {
        return HttpResponseBuilder::new(StatusCode::BAD_REQUEST).body("Invalid receipt data");
    }
    let mut allocation_id = [0u8; 20];
    allocation_id.copy_from_slice(&bytes[..20]);
    let result = indexer_selection::Receipts::receipts_to_voucher(
        &allocation_id.into(),
        data.as_ref(),
        &bytes[20..],
    );
    match result {
        Ok(voucher) => {
            METRICS.collect_receipts_ok.inc();
            tracing::info!(request_size = %bytes.len(), "Collect receipts");
            HttpResponseBuilder::new(StatusCode::OK).json(json!({
                "allocation_id": voucher.allocation_id,
                "fees": voucher.fees.to_string(),
                "signature": format!("0x{}", hex::encode(voucher.signature)),
            }))
        }
        Err(voucher_err) => {
            METRICS.collect_receipts_failed.inc();
            tracing::info!(%voucher_err);
            HttpResponseBuilder::new(StatusCode::BAD_REQUEST).body(voucher_err.to_string())
        }
    }
}

#[tracing::instrument(skip(payload, data))]
async fn handle_network_query(
    _: HttpRequest,
    payload: String,
    data: web::Data<(reqwest::Client, String)>,
) -> HttpResponse {
    let _timer = METRICS.network_subgraph_queries_duration.start_timer();
    let post_request = |body: String| async {
        let response = data
            .0
            .post(&data.1)
            .body(body)
            .header(header::CONTENT_TYPE.as_str(), "application/json")
            .send()
            .await?;
        tracing::info!(network_subgraph_response = %response.status());
        response.text().await
    };
    match post_request(payload).await {
        Ok(result) => {
            METRICS.network_subgraph_queries_ok.inc();
            HttpResponseBuilder::new(StatusCode::OK).body(result)
        }
        Err(network_subgraph_post_err) => {
            tracing::error!(%network_subgraph_post_err);
            METRICS.network_subgraph_queries_failed.inc();
            graphql_error_response(StatusCode::OK, "Failed to process network subgraph query")
        }
    }
}

#[derive(Deserialize, Debug)]
struct QueryBody {
    query: Box<RawValue>,
    variables: Option<Box<RawValue>>,
}

#[tracing::instrument(skip(request, payload, data))]
async fn handle_subgraph_query(
    request: HttpRequest,
    payload: web::Json<QueryBody>,
    data: web::Data<(Config, NetworkResolver, Inputs, &'static AtomicUsize)>,
) -> HttpResponse {
    let query_engine = QueryEngine::new(data.0.clone(), data.1.clone(), data.2.clone());
    let url_params = request.match_info();
    let api_key = url_params.get("api_key").unwrap_or_default();
    let subgraph = if let Some(name) = url_params.get("subgraph_id") {
        Subgraph::Name(name.into())
    } else if let Some(deployment) = url_params
        .get("deployment_id")
        .and_then(|id| id.parse::<SubgraphDeploymentID>().ok())
    {
        Subgraph::Deployment(deployment)
    } else {
        return graphql_error_response(StatusCode::BAD_REQUEST, "Invalid subgraph identifier");
    };
    let query = ClientQuery {
        id: data.3.fetch_add(1, MemoryOrdering::Relaxed) as u64,
        api_key: api_key.to_string(),
        query: payload.query.to_string(),
        variables: payload.variables.as_ref().map(ToString::to_string),
        // TODO: We are assuming mainnet for now.
        network: "mainnet".into(),
        subgraph: subgraph,
    };
    let (query, body) = match query_engine.execute_query(query).await {
        Ok(result) => match serde_json::to_string(&result.response) {
            Ok(body) => (result.query, body),
            Err(err) => return graphql_error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
        },
        Err(err) => return graphql_error_response(StatusCode::OK, format!("{:?}", err)),
    };
    if let Ok(hist) = METRICS
        .query_result_size
        .get_metric_with_label_values(&[&query.indexing.subgraph.ipfs_hash()])
    {
        hist.observe(body.len() as f64);
    }
    HttpResponseBuilder::new(StatusCode::OK)
        .insert_header(header::ContentType::json())
        .body(body)
}

fn graphql_error_response<S: ToString>(status: StatusCode, message: S) -> HttpResponse {
    HttpResponseBuilder::new(status)
        .insert_header(header::ContentType::json())
        .body(json!({"errors": {"message": message.to_string()}}).to_string())
}

#[derive(Clone)]
struct NetworkResolver {
    block_resolvers: Arc<HashMap<String, mpsc::Sender<alchemy_client::Request>>>,
    client: reqwest::Client,
}

#[async_trait]
impl Resolver for NetworkResolver {
    #[tracing::instrument(skip(self, network, unresolved))]
    async fn resolve_blocks(
        &self,
        network: &str,
        unresolved: &[UnresolvedBlock],
    ) -> Vec<BlockHead> {
        use alchemy_client::Request;
        let mut resolved_blocks = Vec::new();
        let resolver = match self.block_resolvers.get(network) {
            Some(resolver) => resolver,
            None => {
                tracing::error!(missing_network = network);
                return resolved_blocks;
            }
        };
        for unresolved_block in unresolved {
            let (sender, receiver) = oneshot::channel();
            if let Err(_) = resolver
                .send(Request::Block(unresolved_block.clone(), sender))
                .await
            {
                tracing::error!("block resolver connection closed");
                return resolved_blocks;
            }
            match receiver.await {
                Ok(resolved) => resolved_blocks.push(resolved),
                Err(_) => {
                    tracing::error!("block resolver connection closed");
                    return resolved_blocks;
                }
            };
        }
        resolved_blocks
    }

    async fn query_indexer(
        &self,
        query: &IndexerQuery,
    ) -> Result<Response<String>, Box<dyn Error>> {
        let receipt = hex::encode(&query.receipt[0..(query.receipt.len() - 64)]);
        self.client
            .post(format!(
                "{}/subgraphs/id/{:?}",
                query.url, query.indexing.subgraph
            ))
            .header("Scalar-Receipt", &receipt)
            .body(query.query.clone())
            .send()
            .await?
            .json::<Response<String>>()
            .await
            .map_err(|err| err.into())
    }
}

#[derive(Clone)]
struct Metrics {
    collect_receipts_duration: prometheus::Histogram,
    collect_receipts_failed: prometheus::IntCounter,
    collect_receipts_ok: prometheus::IntCounter,
    network_subgraph_queries_duration: prometheus::Histogram,
    network_subgraph_queries_failed: prometheus::IntCounter,
    network_subgraph_queries_ok: prometheus::IntCounter,
    query_result_size: prometheus::HistogramVec,
}

lazy_static! {
    static ref METRICS: Metrics = Metrics::new();
}

impl Metrics {
    fn new() -> Self {
        Self {
            collect_receipts_duration: prometheus::register_histogram!(
                "gateway_collect_receipts_duration",
                "Duration of processing requests to collect receipts"
            )
            .unwrap(),
            // TODO: should be renamed to gateway_collect_receipt_requests_failed
            collect_receipts_failed: prometheus::register_int_counter!(
                "gateway_failed_collect_receipt_requests",
                "Failed requests to collect receipts"
            )
            .unwrap(),
            // TODO: should be renamed to gateway_collect_receipt_requests_ok
            collect_receipts_ok: prometheus::register_int_counter!(
                "gateway_collect_receipt_requests",
                "Incoming requests to collect receipts"
            )
            .unwrap(),
            network_subgraph_queries_duration: prometheus::register_histogram!(
                "gateway_network_subgraph_query_duration",
                "Duration of processing a network subgraph query"
            )
            .unwrap(),
            network_subgraph_queries_failed: prometheus::register_int_counter!(
                "gateway_network_subgraph_queries_failed",
                "Network subgraph queries that failed executing"
            )
            .unwrap(),
            network_subgraph_queries_ok: prometheus::register_int_counter!(
                "gateway_network_subgraph_queries_ok",
                "Successfully executed network subgraph queries"
            )
            .unwrap(),
            query_result_size: prometheus::register_histogram_vec!(
                "query_engine_query_result_size",
                "Size of query result",
                &["deployment"]
            )
            .unwrap(),
        }
    }
}
