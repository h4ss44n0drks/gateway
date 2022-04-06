mod block_resolver;
mod ethereum_client;
mod fisherman_client;
mod indexer_client;
mod indexer_selection;
mod ipfs_client;
mod kafka_client;
mod manifest_client;
mod opt;
mod prelude;
mod query_engine;
mod rate_limiter;
mod stats_db;
mod sync_client;
mod vouchers;
mod ws_client;
use crate::{
    block_resolver::{BlockCache, BlockResolver},
    fisherman_client::*,
    indexer_client::IndexerClient,
    ipfs_client::*,
    kafka_client::{ClientQueryResult, IndexerAttempt, KafkaClient, KafkaInterface as _},
    manifest_client::*,
    opt::*,
    prelude::*,
    query_engine::*,
    rate_limiter::*,
};
use actix_cors::Cors;
use actix_web::{
    dev::ServiceRequest,
    http::{header, StatusCode},
    web, App, HttpRequest, HttpResponse, HttpResponseBuilder, HttpServer,
};
use eventuals::EventualExt;
use lazy_static::lazy_static;
use prometheus::{self, Encoder as _};
use reqwest;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::{collections::HashMap, sync::Arc, time::SystemTime};
use structopt::StructOpt as _;
use url::Url;

#[actix_web::main]
async fn main() {
    let opt = Opt::from_args();
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

    let (mut input_writers, inputs) = Inputs::new();

    input_writers
        .indexer_inputs
        .special_indexers
        .write(opt.mips.0);

    // Trigger decay every minute.
    let indexer_selection = inputs.indexers.clone();
    eventuals::timer(Duration::from_secs(60))
        .pipe_async(move |_| {
            let indexer_selection = indexer_selection.clone();
            async move {
                indexer_selection.decay().await;
            }
        })
        .forever();

    let stats_db = match stats_db::create(
        &opt.stats_db_host,
        opt.stats_db_port,
        &opt.stats_db_name,
        &opt.stats_db_user,
        &opt.stats_db_password,
    )
    .await
    {
        Ok(stats_db) => stats_db,
        Err(stats_db_create_err) => {
            tracing::error!(%stats_db_create_err);
            return;
        }
    };
    let block_resolvers = opt
        .ethereum_providers
        .0
        .into_iter()
        .map(|provider| {
            let network = provider.network.clone();
            let (block_cache_writer, block_cache) =
                BlockCache::new(opt.block_cache_head, opt.block_cache_size);
            let chain_client = ethereum_client::create(provider, block_cache_writer);
            let resolver = BlockResolver::new(network.clone(), block_cache, chain_client);
            (network, resolver)
        })
        .collect::<HashMap<String, BlockResolver>>();
    let block_resolvers = Arc::new(block_resolvers);
    let signer_key = opt.signer_key.0;
    let (api_keys_writer, api_keys) = Eventual::new();
    // TODO: argument for timeout
    let sync_metrics = sync_client::create(
        opt.sync_agent,
        Duration::from_secs(30),
        signer_key.clone(),
        input_writers,
        block_resolvers.clone(),
        api_keys_writer,
        opt.sync_agent_accept_empty,
    );
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let ipfs_client = IPFSClient::new(http_client.clone(), opt.ipfs, 5);
    let deployment_ids = inputs
        .deployment_indexers
        .clone()
        .map(|deployments| async move { deployments.keys().cloned().collect() });
    let subgraph_info = manifest_client::create(ipfs_client, deployment_ids);

    let fisherman_client = opt
        .fisherman
        .map(|url| Arc::new(FishermanClient::new(http_client.clone(), url)));
    let subgraph_query_data = SubgraphQueryData {
        config: query_engine::Config {
            indexer_selection_retry_limit: opt.indexer_selection_retry_limit,
            budget_factors: QueryBudgetFactors {
                scale: opt.query_budget_scale,
                discount: opt.query_budget_discount,
                processes: (opt.replica_count * opt.location_count) as f64,
            },
        },
        indexer_client: IndexerClient {
            client: http_client.clone(),
        },
        block_resolvers: block_resolvers.clone(),
        subgraph_info,
        inputs: inputs.clone(),
        api_keys,
        stats_db,
        fisherman_client,
        kafka_client,
    };

    let network_subgraph_query_data = NetworkSubgraphQueryData {
        http_client,
        network_subgraph: opt.network_subgraph,
        network_subgraph_auth_token: opt.network_subgraph_auth_token,
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
    let ip_rate_limiter = RateLimiter::new(
        Duration::from_secs(opt.ip_rate_limit_window_secs.into()),
        opt.ip_rate_limit as usize,
    );
    let api_rate_limiter = RateLimiter::new(
        Duration::from_secs(opt.api_rate_limit_window_secs.into()),
        opt.api_rate_limit as usize,
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
            .app_data(web::Data::new(subgraph_query_data.clone()))
            .app_data(web::JsonConfig::default().error_handler(|err, _| {
                actix_web::error::InternalError::from_response(
                    err,
                    graphql_error_response("Invalid query"),
                )
                .into()
            }))
            .route(
                "/subgraphs/id/{subgraph_id}",
                web::post().to(handle_subgraph_query),
            )
            .route(
                "/deployments/id/{deployment_id}",
                web::post().to(handle_subgraph_query),
            );
        let other = web::scope("")
            .wrap(RateLimiterMiddleware {
                rate_limiter: ip_rate_limiter.clone(),
                key: request_host,
            })
            .route("/", web::get().to(|| async { "Ready to roll!" }))
            .service(
                web::resource("/ready")
                    .app_data(web::Data::new((
                        block_resolvers.clone(),
                        sync_metrics.clone(),
                    )))
                    .route(web::get().to(handle_ready)),
            )
            .service(
                web::resource("/network")
                    .app_data(web::Data::new(network_subgraph_query_data.clone()))
                    .route(web::post().to(handle_network_query)),
            )
            .service(
                web::resource("/collect-receipts")
                    // TODO: decrease payload limit
                    .app_data(web::PayloadConfig::new(16_000_000))
                    .app_data(web::Data::new(signer_key.clone()))
                    .route(web::post().to(vouchers::handle_collect_receipts)),
            )
            .service(
                web::resource("/partial-voucher")
                    .app_data(web::PayloadConfig::new(4_000_000))
                    .app_data(web::Data::new(signer_key.clone()))
                    .route(web::post().to(vouchers::handle_partial_voucher)),
            )
            .service(
                web::resource("/voucher")
                    .app_data(web::Data::new(signer_key.clone()))
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
        &addr[0..addr.rfind(":").unwrap_or(addr.len())])
        // Fallback to hostname
        .unwrap_or_else(|| info.host())
        .to_string()
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

#[tracing::instrument(skip(data))]
async fn handle_ready(
    data: web::Data<(Arc<HashMap<String, BlockResolver>>, sync_client::Metrics)>,
) -> HttpResponse {
    let ready = data
        .0
        .iter()
        .all(|(_, resolver)| resolver.latest_block().is_some())
        && (data.1.allocations.get() > 0);
    if ready {
        HttpResponseBuilder::new(StatusCode::OK).body("Ready")
    } else {
        // Respond with 425 Too Early
        HttpResponseBuilder::new(StatusCode::from_u16(425).unwrap()).body("Not ready")
    }
}

#[derive(Clone)]
struct NetworkSubgraphQueryData {
    http_client: reqwest::Client,
    network_subgraph: String,
    network_subgraph_auth_token: String,
}

#[tracing::instrument(skip(payload, data))]
async fn handle_network_query(
    _: HttpRequest,
    payload: String,
    data: web::Data<NetworkSubgraphQueryData>,
) -> HttpResponse {
    let _timer = METRICS.network_subgraph_queries.duration.start_timer();
    let post_request = |body: String| async {
        let response = data
            .http_client
            .post(&data.network_subgraph)
            .body(body)
            .header(header::CONTENT_TYPE.as_str(), "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", data.network_subgraph_auth_token),
            )
            .send()
            .await?;
        tracing::info!(network_subgraph_response = %response.status());
        response.text().await
    };
    match post_request(payload).await {
        Ok(result) => {
            METRICS.network_subgraph_queries.ok.inc();
            HttpResponseBuilder::new(StatusCode::OK).body(result)
        }
        Err(network_subgraph_post_err) => {
            tracing::error!(%network_subgraph_post_err);
            METRICS.network_subgraph_queries.failed.inc();
            graphql_error_response("Failed to process network subgraph query")
        }
    }
}

#[derive(Deserialize, Debug)]
struct QueryBody {
    query: String,
    variables: Option<Box<RawValue>>,
}

#[derive(Clone)]
struct SubgraphQueryData {
    config: Config,
    indexer_client: IndexerClient,
    fisherman_client: Option<Arc<FishermanClient>>,
    block_resolvers: Arc<HashMap<String, BlockResolver>>,
    subgraph_info: SubgraphInfoMap,
    inputs: Inputs,
    api_keys: Eventual<Ptr<HashMap<String, Arc<APIKey>>>>,
    stats_db: mpsc::UnboundedSender<stats_db::Msg>,
    kafka_client: Arc<KafkaClient>,
}

impl SubgraphQueryData {
    fn resolve_subgraph_deployment(
        &self,
        params: &actix_web::dev::Path<actix_web::dev::Url>,
    ) -> Result<SubgraphDeploymentID, String> {
        if let Some(id) = params.get("subgraph_id") {
            let subgraph = id
                .parse::<SubgraphID>()
                .ok()
                .ok_or_else(|| id.to_string())?;
            self.inputs
                .current_deployments
                .value_immediate()
                .and_then(|map| map.get(&subgraph).cloned())
                .ok_or_else(|| id.to_string())
        } else if let Some(id) = params.get("deployment_id") {
            SubgraphDeploymentID::from_ipfs_hash(id).ok_or_else(|| id.to_string())
        } else {
            Err("".to_string())
        }
    }
}

async fn handle_subgraph_query(
    request: HttpRequest,
    payload: web::Json<QueryBody>,
    data: web::Data<SubgraphQueryData>,
) -> HttpResponse {
    let ray_id = request
        .headers()
        .get("cf-ray")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let variables = payload.variables.as_ref().map(ToString::to_string);
    let mut query = Query::new(ray_id, payload.into_inner().query, variables);
    // We check that the requested subgraph is valid now, since we don't want to log query info for
    // unknown subgraphs requests.
    let deployment = match data.resolve_subgraph_deployment(request.match_info()) {
        Ok(subgraph) => subgraph,
        Err(invalid_subgraph) => {
            tracing::info!(%invalid_subgraph);
            return graphql_error_response("Invalid subgraph identifier");
        }
    };
    query.subgraph = data
        .subgraph_info
        .value_immediate()
        .and_then(|map| map.get(&deployment)?.value_immediate());
    if query.subgraph == None {
        tracing::info!(%deployment);
        return graphql_error_response("Subgraph not found");
    }
    let span = tracing::info_span!(
        "handle_subgraph_query",
        ray_id = %query.ray_id,
        query_id = %query.id,
        %deployment,
        network = %query.subgraph.as_ref().unwrap().network,
    );
    let api_key = request.match_info().get("api_key").unwrap_or("");

    let response = handle_subgraph_query_inner(&request, &data, &mut query, api_key)
        .instrument(span)
        .await;

    let (payload, status_result) = match response {
        Ok(payload) => (payload, Ok(StatusCode::OK.to_string())),
        Err(msg) => (graphql_error_response(&msg), Err(msg)),
    };
    notify_query_result(&data.kafka_client, &query, status_result);

    payload
}

async fn handle_subgraph_query_inner(
    request: &HttpRequest,
    data: &web::Data<SubgraphQueryData>,
    query: &mut Query,
    api_key: &str,
) -> Result<HttpResponse, String> {
    let query_engine = QueryEngine::new(
        data.config.clone(),
        data.indexer_client.clone(),
        data.kafka_client.clone(),
        data.fisherman_client.clone(),
        data.block_resolvers.clone(),
        data.inputs.clone(),
    );
    let api_keys = data.api_keys.value_immediate().unwrap_or_default();
    query.api_key = api_keys.get(api_key).cloned();
    let api_key = match &query.api_key {
        Some(api_key) => api_key.clone(),
        None => {
            METRICS.unknown_api_key.inc();
            return Err("Invalid API key".into());
        }
    };
    if !api_key.queries_activated {
        return Err(
            "Querying not activated yet; make sure to add some GRT to your balance in the studio"
                .into(),
        );
    }
    let domain = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Some(v.parse::<Url>().ok()?.host_str()?.to_string()))
        .unwrap_or("".to_string());
    tracing::debug!(%domain, authorized = ?api_key.domains);
    if !api_key.domains.is_empty()
        && !api_key
            .domains
            .iter()
            .any(|(authorized, _)| domain.starts_with(authorized))
    {
        with_metric(&METRICS.unauthorized_domain, &[&api_key.key], |c| c.inc());
        return Err("Domain not authorized by API key".into());
    }
    let deployment = &query.subgraph.as_ref().unwrap().deployment.clone();
    if !api_key.deployments.is_empty() && !api_key.deployments.contains(&deployment) {
        with_metric(
            &METRICS.queries_unauthorized_deployment,
            &[&api_key.key],
            |counter| counter.inc(),
        );
        return Err("Subgraph not authorized by API key".into());
    }
    if let Err(err) = query_engine.execute_query(query).await {
        return Err(match err {
            QueryEngineError::MalformedQuery => "Invalid query".into(),
            QueryEngineError::NoIndexers => "No indexers found for subgraph deployment".into(),
            QueryEngineError::NoIndexerSelected => {
                "No suitable indexer found for subgraph deployment".into()
            }
            QueryEngineError::FeesTooHigh(count) => {
                format!(
                    "No suitable indexer found, {} indexers requesting higher fees for this query",
                    count
                )
            }
            QueryEngineError::BlockBeforeMin => {
                "Requested block before minimum `startBlock` of subgraph manifest".into()
            }
            QueryEngineError::MissingBlock(_) => "Gateway failed to resolve required blocks".into(),
        });
    }
    let last_attempt = query.indexer_attempts.last().unwrap();
    let response = last_attempt.result.as_ref().unwrap();
    if let Ok(hist) = METRICS
        .query_result_size
        .get_metric_with_label_values(&[&deployment.ipfs_hash()])
    {
        hist.observe(response.payload.len() as f64);
    }
    let _ = data.stats_db.send(stats_db::Msg::AddQuery {
        api_key,
        fee: last_attempt.score.fee,
        domain: domain.to_string(),
        subgraph: query.subgraph.as_ref().unwrap().deployment.ipfs_hash(),
    });
    let attestation = response
        .attestation
        .as_ref()
        .and_then(|attestation| serde_json::to_string(attestation).ok())
        .unwrap_or_default();
    Ok(HttpResponseBuilder::new(StatusCode::OK)
        .insert_header(header::ContentType::json())
        .insert_header(("Graph-Attestation", attestation))
        .body(&response.payload))
}

pub fn graphql_error_response<S: ToString>(message: S) -> HttpResponse {
    HttpResponseBuilder::new(StatusCode::OK)
        .insert_header(header::ContentType::json())
        .body(json!({"errors": {"message": message.to_string()}}).to_string())
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn notify_query_result(kafka_client: &KafkaClient, query: &Query, result: Result<String, String>) {
    let ts = timestamp();
    let query_result = ClientQueryResult::new(&query, result.clone(), ts);
    kafka_client.send(&query_result);

    let indexer_attempts = query
        .indexer_attempts
        .iter()
        .map(|attempt| IndexerAttempt {
            api_key: query_result.api_key.clone(),
            deployment: query_result.deployment.clone(),
            ray_id: query_result.ray_id.clone(),
            indexer: attempt.indexer.to_string(),
            url: attempt.score.url.to_string(),
            allocation: attempt.allocation.to_string(),
            fee: attempt.score.fee.as_f64(),
            utility: *attempt.score.utility,
            blocks_behind: attempt.score.blocks_behind,
            indexer_errors: attempt.indexer_errors.clone(),
            response_time_ms: attempt.duration.as_millis() as u32,
            status: match &attempt.result {
                Ok(response) => response.status.to_string(),
                Err(err) => format!("{:?}", err),
            },
            status_code: attempt.status_code(),
            timestamp: ts,
        })
        .collect::<Vec<IndexerAttempt>>();

    for attempt in indexer_attempts {
        kafka_client.send(&attempt);
    }

    let (status, status_code) = match &result {
        Ok(status) => (status, 0),
        Err(status) => (status, sip24_hash(status) | 0x1),
    };
    let api_key = &query.api_key.as_ref().map(|k| k.key.as_ref()).unwrap_or("");
    let subgraph = query.subgraph.as_ref().unwrap();
    let deployment = subgraph.deployment.to_string();
    tracing::info!(
        ray_id = %query.ray_id,
        query_id = %query.id,
        %deployment,
        network = %query.subgraph.as_ref().unwrap().network,
        %api_key,
        query = %query.query,
        variables = %query.variables.as_deref().unwrap_or(""),
        budget = %query.budget.as_ref().map(ToString::to_string).unwrap_or_default(),
        response_time_ms = (Instant::now() - query.start_time).as_millis() as u32,
        %status,
        status_code,
        "Client query result",
    );
    for (attempt_index, attempt) in query.indexer_attempts.iter().enumerate() {
        let status = match &attempt.result {
            Ok(response) => response.status.to_string(),
            Err(err) => format!("{:?}", err),
        };
        tracing::info!(
            ray_id = %query.ray_id,
            query_id = %query.id,
            api_key = %api_key,
            %deployment,
            attempt_index,
            indexer = %attempt.indexer,
            url = %attempt.score.url,
            allocation = %attempt.allocation,
            fee = %attempt.score.fee,
            utility = *attempt.score.utility,
            blocks_behind = attempt.score.blocks_behind,
            indexer_errors = %attempt.indexer_errors,
            response_time_ms = attempt.duration.as_millis() as u32,
            %status,
            status_code = attempt.status_code() as u32,
            "Indexer attempt",
        );
    }
}

#[derive(Clone)]
struct Metrics {
    network_subgraph_queries: ResponseMetrics,
    query_result_size: prometheus::HistogramVec,
    queries_unauthorized_deployment: prometheus::IntCounterVec,
    unauthorized_domain: prometheus::IntCounterVec,
    unknown_api_key: prometheus::IntCounter,
}

lazy_static! {
    static ref METRICS: Metrics = Metrics::new();
}

impl Metrics {
    fn new() -> Self {
        Self {
            network_subgraph_queries: ResponseMetrics::new(
                "gateway_network_subgraph_query",
                "network subgraph queries",
            ),
            query_result_size: prometheus::register_histogram_vec!(
                "query_engine_query_result_size",
                "Size of query result",
                &["deployment"]
            )
            .unwrap(),
            queries_unauthorized_deployment: prometheus::register_int_counter_vec!(
                "gateway_queries_for_excluded_deployment",
                "Queries for a subgraph deployment not included in an API key",
                &["apiKey"]
            )
            .unwrap(),
            unauthorized_domain: prometheus::register_int_counter_vec!(
                "gateway_queries_from_unauthorized_domain",
                "Queries from a domain not authorized in the API key",
                &["apiKey"],
            )
            .unwrap(),
            unknown_api_key: prometheus::register_int_counter!(
                "gateway_queries_for_unknown_api_key",
                "Queries made against an unknown API key",
            )
            .unwrap(),
        }
    }
}
