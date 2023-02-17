mod block_constraints;
mod chains;
mod client_query;
mod fisherman_client;
mod geoip;
mod indexer_client;
mod indexer_status;
mod ipfs_client;
mod kafka_client;
mod manifest_client;
mod metrics;
mod network_subgraph;
mod opt;
mod price_automation;
mod rate_limiter;
mod receipts;
mod studio_client;
mod subgraph_client;
mod subgraph_deployments;
mod subsciptions_subgraph;
mod subscriptions;
mod unattestable_errors;
mod vouchers;

use crate::{
    chains::*, fisherman_client::*, geoip::GeoIP, indexer_client::IndexerClient,
    indexer_status::IndexingStatus, ipfs_client::*, kafka_client::KafkaClient, opt::*,
    price_automation::QueryBudgetFactors, rate_limiter::*, receipts::ReceiptPools,
};
use actix_cors::Cors;
use actix_web::{
    dev::ServiceRequest,
    http::{header, StatusCode},
    web, App, HttpResponse, HttpResponseBuilder, HttpServer,
};
use anyhow::{self, anyhow};
use clap::Parser as _;
use eventuals::EventualExt as _;
use indexer_selection::{
    actor::{IndexerUpdate, Update},
    BlockStatus, IndexerInfo, Indexing,
};
use network_subgraph::AllocationInfo;
use prelude::{
    buffer_queue::{self, QueueWriter},
    *,
};
use prometheus::{self, Encoder as _};
use secp256k1::SecretKey;
use serde_json::json;
use simple_rate_limiter::RateLimiter;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fs::read_to_string,
    path::Path,
    sync::Arc,
};
use tokio::spawn;

#[actix_web::main]
async fn main() {
    let opt = Opt::parse();
    init_tracing(opt.log_json);
    tracing::info!("Graph gateway starting...");
    tracing::debug!("{:#?}", opt);

    let kafka_client = match KafkaClient::new(&opt.kafka_config()) {
        Ok(kafka_client) => Arc::new(kafka_client),
        Err(kafka_client_err) => {
            tracing::error!(%kafka_client_err);
            return;
        }
    };

    let (isa_state, mut isa_writer) = double_buffer!(indexer_selection::State::default());

    if let Some(path) = &opt.restricted_deployments {
        let restricted_deployments =
            load_restricted_deployments(path).expect("Failed to load restricted deployments");
        tracing::debug!(?restricted_deployments);
        isa_writer
            .update(|indexers| indexers.restricted_deployments = restricted_deployments.clone())
            .await;
    }

    // Start the actor to manage updates
    let (update_writer, update_reader) = buffer_queue::pair();
    spawn(async move {
        indexer_selection::actor::process_updates(isa_writer, update_reader).await;
        tracing::error!("ISA actor stopped");
    });

    let geoip = opt
        .geoip_database
        .filter(|_| !opt.geoip_blocked_countries.is_empty())
        .map(|db| GeoIP::new(db, opt.geoip_blocked_countries).unwrap());

    let block_caches = opt
        .ethereum_providers
        .0
        .into_iter()
        .map(|provider| {
            let network = provider.network.clone();
            let cache = BlockCache::new::<ethereum::Client>(provider);
            (network, cache)
        })
        .collect::<HashMap<String, BlockCache>>();
    let block_caches = Arc::new(block_caches);
    let signer_key = opt.signer_key.0;

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let studio_data =
        studio_client::Actor::create(http_client.clone(), opt.studio_url, opt.studio_auth);
    update_from_eventual(
        studio_data.usd_to_grt,
        update_writer.clone(),
        Update::USDToGRTConversion,
    );

    let network_subgraph_client =
        subgraph_client::Client::new(http_client.clone(), opt.network_subgraph.clone());
    let network_subgraph_data = network_subgraph::Client::create(network_subgraph_client);
    update_from_eventual(
        network_subgraph_data.slashing_percentage,
        update_writer.clone(),
        Update::SlashingPercentage,
    );

    let receipt_pools = ReceiptPools::default();

    let indexer_status_data = indexer_status::Actor::create(
        opt.min_indexer_version,
        geoip,
        network_subgraph_data.indexers.clone(),
    );
    {
        let receipt_pools = receipt_pools.clone();
        let block_caches = block_caches.clone();
        let update_writer = update_writer.clone();
        eventuals::join((
            network_subgraph_data.allocations.clone(),
            network_subgraph_data.indexers,
            indexer_status_data.indexings,
        ))
        .pipe_async(move |(allocations, indexer_info, indexing_statuses)| {
            let receipt_pools = receipt_pools.clone();
            let block_caches = block_caches.clone();
            let update_writer = update_writer.clone();
            async move {
                write_indexer_inputs(
                    &signer_key,
                    &block_caches,
                    &update_writer,
                    &receipt_pools,
                    &allocations,
                    &indexer_info,
                    &indexing_statuses,
                )
                .await;
            }
        })
        .forever();
    }

    let deployment_ids = network_subgraph_data
        .deployment_indexers
        .clone()
        .map(|deployments| async move { deployments.keys().cloned().collect() });
    let ipfs_client = IPFSClient::new(http_client.clone(), opt.ipfs, 50);
    let subgraph_info = manifest_client::create(
        ipfs_client,
        network_subgraph_data.subgraph_deployments.clone(),
        deployment_ids,
    );

    let special_api_keys = Arc::new(HashSet::from_iter(opt.special_api_keys));

    let fisherman_client = opt
        .fisherman
        .map(|url| Arc::new(FishermanClient::new(http_client.clone(), url)));
    let client_query_ctx = client_query::Context {
        indexer_selection_retry_limit: opt.indexer_selection_retry_limit,
        budget_factors: QueryBudgetFactors {
            scale: opt.query_budget_scale,
            discount: opt.query_budget_discount,
            processes: (opt.replica_count * opt.location_count) as f64,
        },
        indexer_client: IndexerClient {
            client: http_client.clone(),
        },
        graph_env_id: opt.graph_env_id.clone(),
        subgraph_info,
        subgraph_deployments: network_subgraph_data.subgraph_deployments,
        deployment_indexers: network_subgraph_data.deployment_indexers,
        api_keys: studio_data.api_keys,
        api_key_payment_required: opt.api_key_payment_required,
        fisherman_client,
        kafka_client,
        block_caches: block_caches.clone(),
        observations: update_writer,
        receipt_pools,
        isa_state,
        special_api_keys,
    };
    let ready_data = ReadyData {
        start_time: Instant::now(),
        block_caches,
        allocations: network_subgraph_data.allocations,
    };

    let metrics_port = opt.metrics_port;
    // Host metrics on a separate server with a port that isn't open to public requests.
    actix_web::rt::spawn(async move {
        HttpServer::new(move || App::new().route("/metrics", web::get().to(handle_metrics)))
            .workers(1)
            .bind(("0.0.0.0", metrics_port))
            .expect("Failed to bind to metrics port")
            .run()
            .await
            .expect("Failed to start metrics server")
    });
    let ip_rate_limiter = RateLimiter::<String>::new(
        opt.ip_rate_limit as usize,
        opt.ip_rate_limit_window_secs as usize,
    );
    let api_rate_limiter = RateLimiter::<String>::new(
        opt.api_rate_limit as usize,
        opt.api_rate_limit_window_secs as usize,
    );
    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_header()
            .allowed_methods(vec!["POST", "OPTIONS"]);
        let api = web::scope("/api/{api_key}")
            .wrap(cors)
            .wrap(RateLimiterMiddleware {
                rate_limiter: api_rate_limiter.clone(),
                key: request_api_key,
            })
            .app_data(web::Data::new(client_query_ctx.clone()))
            .app_data(web::JsonConfig::default().error_handler(|err, _| {
                actix_web::error::InternalError::from_response(
                    err,
                    graphql_error_response("Invalid query"),
                )
                .into()
            }))
            .route(
                "/subgraphs/id/{subgraph_id}",
                web::post().to(client_query::handle_query),
            )
            .route(
                "/deployments/id/{deployment_id}",
                web::post().to(client_query::handle_query),
            );
        let other = web::scope("")
            .wrap(RateLimiterMiddleware {
                rate_limiter: ip_rate_limiter.clone(),
                key: request_host,
            })
            .route("/", web::get().to(|| async { "Ready to roll!" }))
            .service(
                web::resource("/ready")
                    .app_data(web::Data::new(ready_data.clone()))
                    .route(web::get().to(handle_ready)),
            )
            .service(
                web::resource("/collect-receipts")
                    // TODO: decrease payload limit
                    .app_data(web::PayloadConfig::new(16_000_000))
                    .app_data(web::Data::new(signer_key))
                    .route(web::post().to(vouchers::handle_collect_receipts)),
            )
            .service(
                web::resource("/partial-voucher")
                    .app_data(web::PayloadConfig::new(4_000_000))
                    .app_data(web::Data::new(signer_key))
                    .route(web::post().to(vouchers::handle_partial_voucher)),
            )
            .service(
                web::resource("/voucher")
                    .app_data(web::Data::new(signer_key))
                    .route(web::post().to(vouchers::handle_voucher)),
            );
        App::new().service(api).service(other)
    })
    .bind(("0.0.0.0", opt.port))
    .expect("Failed to bind")
    .run()
    .await
    .expect("Failed to start server");
}

fn load_restricted_deployments(
    path: &Path,
) -> anyhow::Result<Arc<HashMap<SubgraphDeploymentID, HashSet<Address>>>> {
    read_to_string(path)?
        .split('\n')
        .filter(|l| l.trim_end() != "")
        .map(|line| {
            let mut csv = line.split_terminator(',');
            let deployment = csv.next()?.parse().ok()?;
            let indexers = csv.map(|i| i.parse().ok()).collect::<Option<_>>()?;
            Some((deployment, indexers))
        })
        .collect::<Option<_>>()
        .map(Arc::new)
        .ok_or(anyhow!("malformed payload"))
}

fn update_from_eventual<V, F>(eventual: Eventual<V>, writer: QueueWriter<Update>, f: F)
where
    V: eventuals::Value,
    F: 'static + Send + Fn(V) -> Update,
{
    eventual
        .pipe(move |v| {
            let _ = writer.write(f(v));
        })
        .forever();
}

fn request_api_key(request: &ServiceRequest) -> String {
    format!(
        "{}/{}",
        request_host(request),
        request.match_info().get("api_key").unwrap_or("")
    )
}

fn request_host(request: &ServiceRequest) -> String {
    let info = request.connection_info();
    info.realip_remote_addr()
        .map(|addr|
        // Trim port number
        &addr[0..addr.rfind(':').unwrap_or(addr.len())])
        // Fallback to hostname
        .unwrap_or_else(|| info.host())
        .to_string()
}

async fn write_indexer_inputs(
    signer: &SecretKey,
    block_caches: &HashMap<String, BlockCache>,
    update_writer: &QueueWriter<Update>,
    receipt_pools: &ReceiptPools,
    allocations: &HashMap<Address, AllocationInfo>,
    indexer_info: &HashMap<Address, Arc<IndexerInfo>>,
    indexing_statuses: &HashMap<Indexing, IndexingStatus>,
) {
    tracing::info!(
        allocations = allocations.len(),
        indexers = indexer_info.len(),
        indexing_statuses = indexing_statuses.len(),
    );

    let mut indexers = indexer_info
        .iter()
        .map(|(indexer, info)| {
            let update = IndexerUpdate {
                info: info.clone(),
                indexings: HashMap::new(),
            };
            (*indexer, update)
        })
        .collect::<HashMap<Address, IndexerUpdate>>();

    let mut latest_blocks = HashMap::<String, u64>::new();
    for (indexing, status) in indexing_statuses {
        let indexer = match indexers.get_mut(&indexing.indexer) {
            Some(indexer) => indexer,
            None => continue,
        };
        let latest = match latest_blocks.entry(status.network.clone()) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => *entry.insert(
                block_caches
                    .get(&status.network)
                    .and_then(|cache| cache.chain_head.value_immediate().map(|b| b.number))
                    .unwrap_or(0),
            ),
        };
        let allocations = allocations
            .iter()
            .filter(|(_, info)| &info.indexing == indexing)
            .map(|(id, info)| (*id, info.allocated_tokens))
            .collect::<HashMap<Address, GRT>>();

        receipt_pools
            .update_receipt_pool(signer, indexing, &allocations)
            .await;

        indexer.indexings.insert(
            indexing.deployment,
            indexer_selection::IndexingStatus {
                allocations: Arc::new(allocations),
                cost_model: status.cost_model.clone(),
                block: Some(BlockStatus {
                    reported_number: status.block.number,
                    blocks_behind: latest.saturating_sub(status.block.number),
                    behind_reported_block: false,
                    min_block: status.min_block,
                }),
            },
        );
    }

    let _ = update_writer.write(Update::Indexers(indexers));
}

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

#[derive(Clone)]
struct ReadyData {
    start_time: Instant,
    block_caches: Arc<HashMap<String, BlockCache>>,
    allocations: Eventual<Ptr<HashMap<Address, AllocationInfo>>>,
}

async fn handle_ready(data: web::Data<ReadyData>) -> HttpResponse {
    // Wait for 30 seconds since startup for subgraph manifests to load.
    let timer_ready = data.start_time.elapsed() > Duration::from_secs(30);
    let block_caches_ready = data
        .block_caches
        .iter()
        .all(|(_, cache)| cache.chain_head.value_immediate().is_some());
    let allocations_ready = data
        .allocations
        .value_immediate()
        .map(|map| map.len())
        .unwrap_or(0)
        > 0;
    if timer_ready && block_caches_ready && allocations_ready {
        HttpResponseBuilder::new(StatusCode::OK).body("Ready")
    } else {
        // Respond with 425 Too Early
        HttpResponseBuilder::new(StatusCode::from_u16(425).unwrap()).body("Not ready")
    }
}

pub fn graphql_error_response<S: ToString>(message: S) -> HttpResponse {
    HttpResponseBuilder::new(StatusCode::OK)
        .insert_header(header::ContentType::json())
        .body(json!({"errors": {"message": message.to_string()}}).to_string())
}
