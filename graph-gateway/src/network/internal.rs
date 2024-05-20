use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use itertools::Itertools;
use semver::Version;
use tokio::sync::Mutex;
use tracing::Instrument;
use url::Url;
use vec1::Vec1;

use crate::{
    indexers,
    network::{
        indexers_addr_blocklist::AddrBlocklist, indexers_cost_model_compiler::CostModelCompiler,
        indexers_cost_model_resolver::CostModelResolver, indexers_host_blocklist::HostBlocklist,
        indexers_host_resolver::HostResolver,
        indexers_indexing_status_resolver::IndexingStatusResolver,
        indexers_poi_blocklist::PoiBlocklist, indexers_poi_resolver::PoiResolver, subgraph,
        subgraph::Client as SubgraphClient, Deployment, GraphNetwork, Indexer, Indexing,
        IndexingId, IndexingStatus, Subgraph,
    },
};

/// An artificial version representing the legacy scalar_tap version.
const LEGACY_SCALAR_TAP_VERSION: Version = Version::new(0, 0, 0);

/// The network topology fetch timeout.
///
/// This timeout is applied independently to the indexers and subgraphs information fetches.
const NETWORK_TOPOLOGY_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The timeout for the indexer's host resolution.
const INDEXER_HOST_RESOLUTION_TIMEOUT: Duration = Duration::from_millis(2_000);

/// The timeout for the indexer's version resolution.
///
/// This timeout is applied independently for the agent and graph node versions fetches.
const INDEXER_VERSION_RESOLUTION_TIMEOUT: Duration = Duration::from_millis(1_500);

/// The timeout for the indexer's POI resolution.
const INDEXER_POI_RESOLUTION_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The timeout for the indexer's indexing status resolution.
const INDEXER_INDEXING_STATUS_RESOLUTION_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The timeout for the indexer's cost model resolution.
const INDEXER_COST_MODEL_RESOLUTION_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Internal types.
pub mod types {
    use std::{collections::HashMap, fmt::Display};

    use alloy_primitives::{Address, BlockNumber};
    use cost_model::CostModel;
    use custom_debug::CustomDebug;
    use eventuals::Ptr;
    use semver::Version;
    use thegraph_core::types::{DeploymentId, SubgraphId};
    use url::Url;
    use vec1::Vec1;

    /// Internal representation of the fetched subgraph information.
    ///
    /// This is not the final representation of the subgraph.
    #[derive(Clone, Debug)]
    pub struct SubgraphInfo {
        pub id: SubgraphId,
        pub id_on_l2: Option<SubgraphId>,
        pub versions: Vec1<SubgraphVersionInfo>,
    }

    #[derive(Clone, Debug)]
    pub struct SubgraphVersionInfo {
        pub version: u32,
        pub deployment: DeploymentInfo,
    }

    /// Internal representation of the fetched deployment information.
    ///
    /// This is not the final representation of the deployment.
    #[derive(Clone, Debug)]
    pub struct DeploymentInfo {
        pub id: DeploymentId,
        pub allocations: Vec1<AllocationInfo>,
        pub manifest_network: String,
        pub manifest_start_block: BlockNumber,
        pub transferred_to_l2: bool,
    }

    /// Internal representation of the fetched allocation information.
    ///
    /// This is not the final representation of the allocation.
    #[derive(Clone, Debug)]
    pub struct AllocationInfo {
        // The allocation ID.
        pub id: Address,
        // The indexer ID.
        pub indexer: Address,
    }

    /// Internal representation of the fetched indexer information.
    ///
    /// This is not the final representation of the indexer.
    #[derive(Clone, CustomDebug)]
    pub struct IndexerInfo {
        /// The indexer's ID.
        pub id: Address,
        /// The indexer's URL.
        ///
        /// It is guaranteed that the URL scheme is either HTTP or HTTPS and the URL has a host.
        #[debug(with = Display::fmt)]
        pub url: Url,
        /// The total amount of tokens staked by the indexer.
        pub staked_tokens: u128,
        /// The list of deployments the indexer is associated with.
        ///
        /// The deployments are ordered from highest to lowest associated token allocation.
        //  See ref: d260724b-a445-4842-964e-fb95062c119d
        pub deployments: Vec1<DeploymentId>,
        /// The indexer's "indexer service" version.
        pub indexer_agent_version: Version,
        /// The indexer's "scalar_tap" version.
        pub scalar_tap_version: Version,
        /// The indexer's "graph node" version.
        pub graph_node_version: Version,

        /// A flag indicating if the indexer is using the legacy scalar.
        pub legacy_scalar: bool,

        /// The largest allocation per indexing.
        pub largest_allocation: HashMap<DeploymentId, Address>,
        /// The total amount of tokens allocated by the indexer per indexing.
        pub total_allocated_tokens: HashMap<DeploymentId, u128>,

        /// The indexer's indexing status.
        pub indexings_status: HashMap<DeploymentId, IndexerIndexingStatusInfo>,
        /// The indexer's indexings cost models.
        pub indexings_cost_models: HashMap<DeploymentId, Ptr<CostModel>>,
    }

    /// Internal representation of the fetched indexer indexing status information and cost models.
    #[derive(Clone, Debug)]
    pub struct IndexerIndexingStatusInfo {
        /// The latest block the indexer has indexed for the deployment.
        pub latest_block: BlockNumber,
        /// The minimum block the indexer has indexed for the deployment.
        pub min_block: Option<BlockNumber>,
    }
}

/// Internal type holding the network service state.
pub struct InternalState {
    pub indexers_http_client: reqwest::Client,
    pub indexers_min_agent_version: Version,
    pub indexers_min_scalar_tap_version: Version,
    pub indexers_min_graph_node_version: Version,
    pub indexers_addr_blocklist: Option<AddrBlocklist>,
    pub indexers_host_resolver: Mutex<HostResolver>,
    pub indexers_host_blocklist: Option<HostBlocklist>,
    pub indexers_pois_blocklist: Option<(PoiBlocklist, Mutex<PoiResolver>)>,
    pub indexers_indexing_status_resolver: IndexingStatusResolver,
    pub indexers_cost_model_resolver: (CostModelResolver, Mutex<CostModelCompiler>),
}

/// Fetch the network topology information from the graph network subgraph.
pub async fn fetch_update(
    client: &Mutex<SubgraphClient>,
    state: &InternalState,
) -> anyhow::Result<GraphNetwork> {
    // Fetch and pre-process the network topology information
    let (indexers, subgraphs) = futures::future::try_join(
        async {
            let indexers = {
                let mut subgraph_client = client.lock().await;
                match tokio::time::timeout(
                    NETWORK_TOPOLOGY_FETCH_TIMEOUT,
                    fetch_and_pre_process_indexers_info(&mut subgraph_client),
                )
                .await
                {
                    // If the fetch timed out, return an error
                    Err(_) => Err(anyhow!("indexers info fetch timed out")),
                    Ok(resp) => match resp {
                        // If the fetch failed, return an error
                        Err(err) => Err(anyhow!("indexers info fetch failed: {err}")),
                        Ok(resp) => Ok(resp),
                    },
                }
            }?;

            // Process the fetched network topology information
            process_indexers_info(state, indexers).await
        },
        async {
            let mut subgraph_client = client.lock().await;
            match tokio::time::timeout(
                NETWORK_TOPOLOGY_FETCH_TIMEOUT,
                fetch_and_pre_process_subgraphs_info(&mut subgraph_client),
            )
            .await
            {
                // If the fetch timed out, return an error
                Err(_) => Err(anyhow!("subgraphs info fetch timed out")),
                Ok(resp) => match resp {
                    // If the fetch failed, return an error
                    Err(err) => Err(anyhow!("subgraphs info fetch failed: {err}")),
                    Ok(resp) => Ok(resp),
                },
            }
        },
    )
    .await?;

    Ok(construct_network_topology_snapshot(indexers, subgraphs))
}

/// Fetch the indexers information from the graph network subgraph and performs pre-processing
/// steps, i.e., validation and conversion into the internal representation.
///
///   1. Fetch the indexers information from the graph network subgraph.
///   2. Validate and convert the indexers fetched info into the internal representation.
///
/// If the fetch fails or the response is empty, an error is returned.
///
/// Invalid info is filtered out before converting into the internal representation. If no valid
/// indexers are found, an error is returned.
pub async fn fetch_and_pre_process_indexers_info(
    client: &mut SubgraphClient,
) -> anyhow::Result<Vec1<types::IndexerInfo>> {
    // Fetch the indexers information from the graph network subgraph
    let indexers = client
        .fetch_indexers()
        .await
        .map_err(|err| anyhow!("indexers fetch failed: {err}"))?;
    if indexers.is_empty() {
        return Err(anyhow!("empty indexers fetch"));
    }

    // Map the fetched indexers info into the internal representation
    // If no valid indexers are found, an error is returned.
    let indexers = indexers
        .into_iter()
        .filter_map(|indexer| {
            let _span =
                tracing::debug_span!("indexer pre-processing", indexer.id = %indexer.id).entered();
            tracing::trace!(
                indexer.allocations_count = %indexer.allocations.len(),
                indexer.url = ?indexer.url,
            );

            match try_into_internal_indexer_info(indexer) {
                Ok(indexer) => Some(indexer),
                Err(err) => {
                    tracing::debug!("filtering-out indexer: {err}");
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no valid indexers found"))?;

    Ok(indexers)
}

/// Fetch the subgraphs information from the graph network subgraph and performs pre-processing
/// steps, i.e., validation and conversion into the internal representation.
///
///   1. Fetch the subgraphs information from the graph network subgraph.
///   2. Validate and convert the subgraphs fetched info into the internal representation.
///
/// If the fetch fails or the response is empty, an error is returned.
///
/// Invalid info is filtered out before converting into the internal representation. If no valid
/// subgraphs are found, an error is returned.
pub async fn fetch_and_pre_process_subgraphs_info(
    client: &mut SubgraphClient,
) -> anyhow::Result<Vec1<types::SubgraphInfo>> {
    // Fetch the subgraphs information from the graph network subgraph
    let subgraphs = client
        .fetch_subgraphs()
        .await
        .map_err(|err| anyhow!("subgraphs fetch failed: {err}"))?;
    if subgraphs.is_empty() {
        return Err(anyhow!("empty subgraphs fetch"));
    }

    // Map the fetched subgraphs info into the internal representation
    // If no valid subgraphs are found, an error is returned.
    let subgraphs = subgraphs
        .into_iter()
        .filter_map(|subgraph| {
            let _span = tracing::debug_span!(
                "subgraph pre-processing",
                subgraph.id = %subgraph.id,
            )
            .entered();
            match try_into_internal_subgraph_info(subgraph) {
                Ok(subgraph) => Some(subgraph),
                Err(err) => {
                    tracing::debug!("filtering-out subgraph: {err}");
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no valid subgraphs found"))?;

    Ok(subgraphs)
}

/// Convert from the fetched indexer information into the internal representation.
///
/// If the indexer is invalid, e.g., has no URL or no allocations, an error is returned.
fn try_into_internal_indexer_info(
    indexer: subgraph::types::fetch_indexers::Indexer,
) -> anyhow::Result<types::IndexerInfo> {
    // Check if the indexer is present
    let indexer_url = indexer.url.ok_or_else(|| anyhow!("missing URL"))?;

    // Parse the URL. It must have an HTTP (or HTTPS) scheme and a valid host.
    // Filter out indexers with invalid URLs.
    let indexer_url = indexer_url
        .parse::<Url>()
        .map_err(|err| anyhow!("URL parsing failed: {err}"))?;
    if !indexer_url.scheme().starts_with("http") {
        return Err(anyhow!("invalid URL: invalid scheme"));
    }
    if indexer_url.host().is_none() {
        return Err(anyhow!("invalid URL: missing host"));
    }

    // Check if the indexer has any allocations
    let indexer_allocations: Vec1<_> = indexer
        .allocations
        .try_into()
        .map_err(|_| anyhow!("no allocations"))?;

    // Get the list of unique deployment IDs the indexer is associated with.
    // NOTE: The indexer is guaranteed to have at least one allocation and one
    // deployment.
    // See ref: d260724b-a445-4842-964e-fb95062c119d
    let indexer_deployment_ids: Vec1<_> = indexer_allocations
        .iter()
        .map(|alloc| alloc.subgraph_deployment.id)
        .unique()
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no deployments"))?;

    // Get the largest allocation and the total amount of tokens allocated for each indexing
    // NOTE: The allocations are ordered by `allocatedTokens` in descending order, and
    // the largest allocation is the first one.
    // See ref: d260724b-a445-4842-964e-fb95062c119d
    let indexer_indexing_largest_allocations = indexer_deployment_ids
        .iter()
        .flat_map(|deployment_id| {
            indexer_allocations
                .iter()
                .filter_map(|alloc| {
                    if alloc.subgraph_deployment.id == *deployment_id {
                        Some((*deployment_id, alloc.id))
                    } else {
                        None
                    }
                })
                .next()
        })
        .collect::<HashMap<_, _>>();

    let indexer_indexing_total_allocated_tokens = indexer_deployment_ids
        .iter()
        .map(|deployment_id| {
            let total = indexer_allocations
                .iter()
                .filter_map(|alloc| {
                    if alloc.subgraph_deployment.id == *deployment_id {
                        Some(alloc.allocated_tokens)
                    } else {
                        None
                    }
                })
                .sum();
            (*deployment_id, total)
        })
        .collect::<HashMap<_, _>>();

    Ok(types::IndexerInfo {
        id: indexer.id,
        url: indexer_url,
        staked_tokens: indexer.staked_tokens,
        deployments: indexer_deployment_ids,
        largest_allocation: indexer_indexing_largest_allocations,
        total_allocated_tokens: indexer_indexing_total_allocated_tokens,
        indexer_agent_version: Version::new(0, 0, 0), // Placeholder
        scalar_tap_version: Version::new(0, 0, 0),    // Placeholder
        graph_node_version: Version::new(0, 0, 0),    // Placeholder
        legacy_scalar: false,                         // Placeholder
        indexings_status: HashMap::new(),             // Placeholder
        indexings_cost_models: HashMap::new(),        // Placeholder
    })
}

/// Convert from the fetched subgraph information into the internal representation.
///
/// The subgraph must have at least one valid deployment to be considered valid. And each
/// deployment must have a valid manifest and at least one allocation to be considered valid.
///
/// If the subgraph is invalid, e.g., no valid deployments found, an error is returned.
fn try_into_internal_subgraph_info(
    subgraph: subgraph::types::fetch_subgraphs::Subgraph,
) -> anyhow::Result<types::SubgraphInfo> {
    let versions = subgraph
        .versions
        .into_iter()
        .filter_map(|version| {
            let deployment = version.subgraph_deployment;

            let _span = tracing::trace_span!(
                "subgraph-versions",
                subgraph = %subgraph.id,
                version.number = %version.version,
                version.deployment = %deployment.id,
            )
            .entered();

            // Deployment must have a valid manifest to be considered valid.
            let (deployment_manifest_network, deployment_manifest_start_block) = match deployment
                .manifest
            {
                // Deployment must have a valid manifest to be considered valid.
                None => {
                    tracing::trace!("filtering-out version-deployment: no manifest");
                    return None;
                }
                Some(manifest) if manifest.network.is_none() => {
                    tracing::trace!("filtering-out version-deployment: no manifest network info");
                    return None;
                }
                Some(manifest) => (manifest.network?, manifest.start_block.unwrap_or(0)),
            };

            // Deployment must have at least one allocation to be considered valid.
            let deployment_allocations = deployment
                .allocations
                .into_iter()
                .map(|allocation| types::AllocationInfo {
                    id: allocation.id,
                    indexer: allocation.indexer.id,
                })
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| anyhow!("no allocations"));
            let deployment_allocations = match deployment_allocations {
                Ok(allocations) => allocations,
                Err(err) => {
                    tracing::trace!("filtering-out version-deployment: {err}");
                    return None;
                }
            };

            let deployment_id = deployment.id;
            let deployment_transferred_to_l2 = deployment.transferred_to_l2;

            let version_number = version.version;
            let version_deployment = types::DeploymentInfo {
                id: deployment_id,
                allocations: deployment_allocations,
                manifest_network: deployment_manifest_network,
                manifest_start_block: deployment_manifest_start_block,
                transferred_to_l2: deployment_transferred_to_l2,
            };

            Some(types::SubgraphVersionInfo {
                version: version_number,
                deployment: version_deployment,
            })
        })
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no valid versions found"))?;

    Ok(types::SubgraphInfo {
        id: subgraph.id,
        id_on_l2: subgraph.id_on_l2,
        versions,
    })
}

/// Process the fetched network topology information.
pub async fn process_indexers_info(
    state: &InternalState,
    indexers: Vec1<types::IndexerInfo>,
) -> anyhow::Result<Vec1<types::IndexerInfo>> {
    // Process the fetched indexers information
    let indexers_info = {
        let indexers_iter_fut = indexers.into_iter().map(move |indexer| {
            // Instrument the indexer processing span
            let indexer_span = tracing::debug_span!(
                "indexer processing",
                indexer.id = %indexer.id,
                indexer.url = %indexer.url,
                indexer.agent_version = tracing::field::Empty,
                indexer.graph_node_version = tracing::field::Empty,
                indexer.scalar_tap_version = tracing::field::Empty,
            );
            tracing::trace!(parent: &indexer_span, "processing");

            async move {
                let mut indexer = indexer;

                // Check if the indexer's address is in the address blocklist
                if let Err(err) = check_indexer_blocked_by_addr_blocklist(
                    &state.indexers_addr_blocklist,
                    &indexer,
                ) {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Check if the indexer's host is in the host blocklist
                if let Err(err) = check_indexer_blocked_by_host_blocklist(
                    &state.indexers_host_resolver,
                    &state.indexers_host_blocklist,
                    &indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Check if the indexer's reported versions are supported
                if let Err(err) = check_indexer_blocked_by_version(
                    &state.indexers_http_client,
                    &state.indexers_min_agent_version,
                    &state.indexers_min_graph_node_version,
                    &state.indexers_min_scalar_tap_version,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Update the span information with the resolved versions
                tracing::Span::current()
                    .record(
                        "indexer.agent_version",
                        tracing::field::display(&indexer.indexer_agent_version),
                    )
                    .record(
                        "indexer.graph_node_version",
                        tracing::field::display(&indexer.graph_node_version),
                    )
                    .record(
                        "indexer.scalar_tap_version",
                        tracing::field::display(&indexer.scalar_tap_version),
                    );

                // Check if the indexer's deployments should be blocked by POI
                // Update the indexer's deployments list to only include the deployments that are
                // not blocked by POI. If the indexer has no deployments left, it must be ignored.
                if let Err(err) =
                    check_indexer_blocked_by_poi(&state.indexers_pois_blocklist, &mut indexer).await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                // Fetch the indexer's indexing statuses and cost models
                // NOTE: At this point, the indexer's deployments list should contain only the
                //       deployment IDs that were not blocked by any blocklist.
                if let Err(err) = resolve_indexer_indexing_status_and_cost_models(
                    &state.indexers_indexing_status_resolver,
                    &state.indexers_cost_model_resolver,
                    &mut indexer,
                )
                .await
                {
                    tracing::debug!("filtering-out indexer: {err}");
                    return None;
                }

                Some(indexer)
            }
            .instrument(indexer_span)
        });

        // Wait for all the indexers to be processed
        futures::future::join_all(indexers_iter_fut).await
    };
    indexers_info
        .into_iter()
        .flatten() // Filter out the `None` values
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow!("no valid indexers found"))
}

/// Check if the indexer's address is in the address blocklist.
///
/// - If the address blocklist was not configured: the indexer is ALLOWED.
/// - If the address is in the blocklist: the indexer is BLOCKED.
fn check_indexer_blocked_by_addr_blocklist(
    addr_blocklist: &Option<AddrBlocklist>,
    indexer: &types::IndexerInfo,
) -> anyhow::Result<()> {
    let blocklist = match addr_blocklist {
        Some(blocklist) => blocklist,
        None => return Ok(()),
    };

    // Check if the indexer's address is in the blocklist
    if blocklist.check(&indexer.id).is_blocked() {
        return Err(anyhow!("indexer address blocked by blocklist"));
    }

    Ok(())
}

/// Resolve and check if the indexer's host is in the host blocklist.
///
/// - If the indexer's host is not resolvable: the indexer is BLOCKED.
/// - If the host blocklist was not configured: the indexer is ALLOWED.
/// - If the indexer's host is in the blocklist: the indexer is BLOCKED.
async fn check_indexer_blocked_by_host_blocklist(
    host_resolver: &Mutex<HostResolver>,
    host_blocklist: &Option<HostBlocklist>,
    indexer: &types::IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's URL, if it fails (or times out), the indexer must be BLOCKED
    let mut host_resolver = host_resolver.lock().await;
    let resolution_result = match tokio::time::timeout(
        INDEXER_HOST_RESOLUTION_TIMEOUT,
        host_resolver.resolve_url(&indexer.url),
    )
    .await
    {
        // If the resolution timed out, the indexer must be BLOCKED
        Err(_) => {
            return Err(anyhow!("indexer URL resolution timed out"));
        }
        Ok(res) => match res {
            // If the resolution failed, the indexer must be BLOCKED
            Err(err) => {
                return Err(anyhow!("failed to resolve indexer URL: {err}"));
            }
            Ok(result) => result,
        },
    };

    // If the host blocklist was not configured, the indexer must be ALLOWED
    let host_blocklist = match host_blocklist {
        Some(blocklist) => blocklist,
        _ => return Ok(()),
    };

    if host_blocklist.check(&resolution_result).is_blocked() {
        return Err(anyhow!("indexer host blocked by blocklist"));
    }

    Ok(())
}

/// Resolve and check if the indexer's reported versions are supported.
///
/// - If the agent version is not resolvable: the indexer must be BLOCKED.
/// - If the agent version is below the minimum required: the indexer must be BLOCKED.
/// - If the graph node version is not resolvable: the indexer must be BLOCKED.
/// - If the graph node version is below the minimum required: the indexer must be BLOCKED.
async fn check_indexer_blocked_by_version(
    http_client: &reqwest::Client,
    min_agent_version: &Version,
    min_graph_node_version: &Version,
    min_scalar_tap_version: &Version,
    indexer: &mut types::IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's agent version
    let indexer_agent_version_url = indexers::version_url(&indexer.url);
    let agent_version = match tokio::time::timeout(
        INDEXER_VERSION_RESOLUTION_TIMEOUT,
        indexers::version::query_indexer_service_version(http_client, indexer_agent_version_url),
    )
    .await
    {
        // If the resolution timed out, the indexer must be BLOCKED
        Err(_) => {
            return Err(anyhow!("agent version resolution timed out"));
        }
        Ok(res) => match res {
            // If the resolution failed, the indexer must be BLOCKED
            Err(err) => {
                return Err(anyhow!("agent version resolution failed: {err}"));
            }
            Ok(result) => result,
        },
    };

    // Check if the indexer's agent version is supported
    if agent_version < *min_agent_version {
        return Err(anyhow!(
            "agent version {} below the minimum required {}",
            agent_version,
            min_agent_version
        ));
    }

    // Resolve the indexer's scalar_tap version
    // TODO: Resolve the indexers scalar_tap version
    //  For now, set the scalar_tap version to the agent version if it is above the minimum required
    //  version, otherwise, set it to the legacy scalar_tap version.
    let scalar_tap_version = if agent_version > *min_scalar_tap_version {
        agent_version.clone()
    } else {
        LEGACY_SCALAR_TAP_VERSION.clone()
    };

    // Resolve the indexer's graph node version, with a timeout
    let indexer_graph_node_version_url = indexers::status_url(&indexer.url);
    let graph_node_version = match tokio::time::timeout(
        INDEXER_VERSION_RESOLUTION_TIMEOUT,
        indexers::version::query_graph_node_version(http_client, indexer_graph_node_version_url),
    )
    .await
    {
        // If the resolution timed out, the indexer must be BLOCKED
        Err(_) => {
            return Err(anyhow!("graph-node version resolution timed out"));
        }
        Ok(result) => match result {
            // If the resolution failed, the indexer must be BLOCKED
            Err(err) => {
                // TODO: After more graph nodes support reporting their version,
                //  we should assume they are on the minimum version if we can't
                //  get the version.
                tracing::trace!("graph-node version resolution failed: {err}");
                min_graph_node_version.clone()
            }
            Ok(result) => result,
        },
    };

    // Check if the indexer's graph node version is supported
    if graph_node_version < *min_graph_node_version {
        return Err(anyhow!(
            "Graph node version {} below the minimum required {}",
            graph_node_version,
            min_graph_node_version
        ));
    }

    // Set the indexer's versions
    indexer.indexer_agent_version = agent_version;
    indexer.scalar_tap_version = scalar_tap_version;
    indexer.graph_node_version = graph_node_version;

    indexer.legacy_scalar = indexer.scalar_tap_version == LEGACY_SCALAR_TAP_VERSION;

    Ok(())
}

/// Resolve and check if any of the indexer's deployments should be blocked by POI.
///
/// - If the POI blocklist was not configured: the indexer must be ALLOWED.
/// - If not indexing any of the affected deployments: the indexer must be ALLOWED.
/// - If there are no healthy indexings, i.e., all indexings are blocked: the indexer must be BLOCKED.
async fn check_indexer_blocked_by_poi(
    pois_blocklist: &Option<(PoiBlocklist, Mutex<PoiResolver>)>,
    indexer: &mut types::IndexerInfo,
) -> anyhow::Result<()> {
    // If the POI blocklist was not configured, the indexer must be ALLOWED
    let (pois_blocklist, pois_resolver) = match pois_blocklist {
        Some((blocklist, resolver)) => (blocklist, resolver),
        _ => return Ok(()),
    };

    // Get the list of affected POIs to resolve for the indexer's deployments
    // If none of the deployments are affected, the indexer must be ALLOWED
    let indexer_affected_pois = pois_blocklist.affected_pois_metadata(&indexer.deployments);
    if indexer_affected_pois.is_empty() {
        return Ok(());
    }

    // Resolve the indexer public POIs for the affected deployments
    let indexer_status_url = indexers::status_url(&indexer.url);
    let mut pois_resolver = pois_resolver.lock().await;
    let poi_result = match tokio::time::timeout(
        INDEXER_POI_RESOLUTION_TIMEOUT,
        pois_resolver.resolve_indexer_public_pois(indexer_status_url, indexer_affected_pois),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return Err(anyhow!("indexer POI resolution timed out"));
        }
    };

    // Check if any of the reported POIs are in the blocklist. and filter out the indexings
    // Update the indexers deployments to only include the deployments that are not affected
    // i.e., keep the deployments that are not blocked by POI.
    let check_result = pois_blocklist.check(poi_result);
    indexer
        .deployments
        .retain(|id| match check_result.get(id) {
            Some(state) => state.is_allowed(),
            None => {
                // If the deployment is not affected, keep it
                true
            }
        })
        // If all deployments are blocked, the indexer must be BLOCKED
        .map_err(|_| anyhow!("all deployments blocked due to blocked POIs"))?;

    Ok(())
}

/// Resolve the indexer's indexing status and cost models.
async fn resolve_indexer_indexing_status_and_cost_models(
    indexing_status_resolver: &IndexingStatusResolver,
    (resolver, compiler): &(CostModelResolver, Mutex<CostModelCompiler>),
    indexer: &mut types::IndexerInfo,
) -> anyhow::Result<()> {
    // Resolve the indexer's indexing status
    let indexer_status_url = indexers::status_url(&indexer.url);
    let indexings_status = match tokio::time::timeout(
        INDEXER_INDEXING_STATUS_RESOLUTION_TIMEOUT,
        indexing_status_resolver.resolve(indexer_status_url, &indexer.deployments),
    )
    .await
    {
        // If the resolution timed out, the indexer must be BLOCKED
        Err(_) => {
            return Err(anyhow!("indexing status resolution timed out"));
        }
        Ok(status) => match status {
            // If the resolution failed, the indexer must be BLOCKED
            Err(err) => {
                return Err(anyhow!("indexing status resolution failed: {err}"));
            }
            Ok(result) => result,
        },
    };
    tracing::trace!(
        indexings = %indexer.deployments.len(),
        indexing_status = %indexings_status.len(),
        "indexing status resolved"
    );

    // Resolve the indexer's cost model sources
    let indexer_cost_url = indexers::cost_url(&indexer.url);
    let indexings_cost_models = match tokio::time::timeout(
        INDEXER_COST_MODEL_RESOLUTION_TIMEOUT,
        resolver.resolve(indexer_cost_url, &indexer.deployments),
    )
    .await
    {
        // If the resolution timed out, the indexer must be BLOCKED
        Err(_) => {
            return Err(anyhow!("cost model resolution timed out"));
        }
        Ok(result) => result,
    };

    // Compile the cost model sources into cost models
    let indexings_cost_models = if !indexings_cost_models.is_empty() {
        let mut compiler = compiler.lock().await;

        indexings_cost_models
            .into_iter()
            .filter_map(|(deployment, source)| match compiler.compile(source) {
                Err(err) => {
                    tracing::debug!("cost model compilation failed: {err}");
                    None
                }
                Ok(cost_model) => Some((deployment, cost_model)),
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Construct the indexings table with the resolved status and cost models
    let indexings_status = indexings_status
        .into_iter()
        .map(|(deployment_id, res)| {
            (
                deployment_id,
                types::IndexerIndexingStatusInfo {
                    latest_block: res.latest_block,
                    min_block: res.min_block,
                },
            )
        })
        .collect();

    // Set the indexer's indexing status and cost models
    indexer.indexings_status = indexings_status;
    indexer.indexings_cost_models = indexings_cost_models;

    Ok(())
}

/// Construct the [`GraphNetwork`] from the fetched and processed information.
pub fn construct_network_topology_snapshot(
    indexers: Vec1<types::IndexerInfo>,
    subgraphs: Vec1<types::SubgraphInfo>,
) -> GraphNetwork {
    // Construct the indexers info table
    let indexers_info_table = indexers
        .into_iter()
        .map(|indexer| (indexer.id, indexer))
        .collect::<HashMap<_, _>>();

    // Construct the deployments info table
    let deployments_info_table = subgraphs
        .iter()
        .flat_map(|subgraph| {
            subgraph
                .versions
                .iter()
                .map(|version| (version.deployment.id, version.deployment.clone()))
        })
        .collect::<HashMap<_, _>>();

    // Construct the subgraphs info table
    let subgraphs_info_table = subgraphs
        .into_iter()
        .map(|subgraph| (subgraph.id, subgraph))
        .collect::<HashMap<_, _>>();

    // Construct the indexings indexers table
    let indexings_indexers_table = indexers_info_table
        .iter()
        .map(|(indexer_id, indexer)| {
            (
                indexer_id,
                Arc::new(Indexer {
                    id: indexer.id,
                    url: indexer.url.clone(),
                    indexer_agent_version: indexer.indexer_agent_version.clone(),
                    graph_node_version: indexer.graph_node_version.clone(),
                    legacy_scalar: indexer.legacy_scalar,
                    indexings: indexer.deployments.iter().copied().collect(),
                    staked_tokens: indexer.staked_tokens,
                }),
            )
        })
        .collect::<HashMap<_, _>>();

    // Construct the topology subgraphs table
    let subgraphs = subgraphs_info_table
        .into_iter()
        .filter_map(|(subgraph_id, subgraph)| {
            let highest_version = subgraph.versions.first();
            let highest_version_number = highest_version.version;
            let highest_version_deployment_manifest_chain =
                highest_version.deployment.manifest_network.clone();
            let highest_version_deployment_manifest_start_block =
                highest_version.deployment.manifest_start_block;

            let versions_behind_table = subgraph
                .versions
                .iter()
                .map(|version| {
                    let deployment_id = version.deployment.id;
                    let deployment_versions_behind = highest_version_number
                        .saturating_sub(version.version)
                        .try_into()
                        .unwrap_or(u8::MAX);
                    (deployment_id, deployment_versions_behind)
                })
                .collect::<HashMap<_, _>>();

            // If all the subgraph's version deployments have been transferred to L2, mark the
            // subgraph as transferred to L2.
            let subgraph_transferred_to_l2 = subgraph
                .versions
                .iter()
                .all(|version| version.deployment.transferred_to_l2);
            let subgraph_id_on_l2 = subgraph.id_on_l2;

            let subgraph_indexings = subgraph
                .versions
                .into_iter()
                .flat_map(|version| {
                    let deployment_id = version.deployment.id;
                    let indexing_deployment_versions_behind = versions_behind_table
                        .get(&deployment_id)
                        .copied()
                        .unwrap_or(u8::MAX);

                    version
                        .deployment
                        .allocations
                        .into_iter()
                        .filter_map(|alloc| {
                            // If the indexer is not in the indexers table, exclude it. It might
                            // have been filtered out due to different reasons, e.g., invalid info.
                            let indexing_indexer_id = alloc.indexer;
                            let indexing_indexer_info =
                                indexers_info_table.get(&indexing_indexer_id)?;

                            // The indexer deployments list contains the healthy deployments. It
                            // must contain the deployment ID, otherwise, that means it was filtered
                            // out, e.g., invalid POI blocklist, etc.
                            if !indexing_indexer_info.deployments.contains(&deployment_id) {
                                return None;
                            }

                            let indexing_indexer =
                                indexings_indexers_table.get(&indexing_indexer_id)?;

                            // If the indexing has no allocations, exclude it
                            let indexing_largest_allocation_addr = indexing_indexer_info
                                .largest_allocation
                                .get(&deployment_id)?;

                            // If the indexing has no total allocated tokens, exclude it
                            let indexing_total_allocated_tokens = indexing_indexer_info
                                .total_allocated_tokens
                                .get(&deployment_id)?;

                            let indexing_status = indexing_indexer_info
                                .indexings_status
                                .get(&deployment_id)
                                .map(|status| IndexingStatus {
                                    latest_block: status.latest_block,
                                    min_block: status.min_block,
                                });

                            let indexing_cost_model = indexing_indexer_info
                                .indexings_cost_models
                                .get(&deployment_id)
                                .cloned();

                            let indexing_id = IndexingId {
                                indexer: indexing_indexer_id,
                                deployment: deployment_id,
                            };
                            let indexing = Indexing {
                                id: indexing_id,
                                versions_behind: indexing_deployment_versions_behind,
                                largest_allocation: *indexing_largest_allocation_addr,
                                total_allocated_tokens: *indexing_total_allocated_tokens,
                                indexer: indexing_indexer.clone(),
                                status: indexing_status,
                                cost_model: indexing_cost_model,
                            };
                            Some((indexing_id, indexing))
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<HashMap<_, _>>();
            if subgraph_indexings.is_empty() {
                return None;
            }

            let subgraph_deployments = subgraph_indexings
                .keys()
                .map(|indexing_id| indexing_id.deployment)
                .collect::<HashSet<_>>();
            if subgraph_deployments.is_empty() {
                return None;
            }

            Some((
                subgraph_id,
                Arc::new(Subgraph {
                    id: subgraph.id,
                    l2_id: subgraph_id_on_l2,
                    transferred_to_l2: subgraph_transferred_to_l2,
                    chain: highest_version_deployment_manifest_chain,
                    start_block: highest_version_deployment_manifest_start_block,
                    deployments: subgraph_deployments,
                    indexings: subgraph_indexings,
                }),
            ))
        })
        .collect::<HashMap<_, _>>();

    // Construct the deployments table
    let deployments = deployments_info_table
        .into_iter()
        .filter_map(|(deployment_id, deployment)| {
            let deployment_versions_behind = 0;
            let deployment_manifest_chain = deployment.manifest_network.clone();
            let deployment_manifest_start_block = deployment.manifest_start_block;

            let deployment_indexings = deployment
                .allocations
                .into_iter()
                .filter_map(|alloc| {
                    // If the indexer is not in the indexers table, exclude it. It might
                    // have been filtered out due to different reasons, e.g., invalid info.
                    let indexing_indexer_id = alloc.indexer;
                    let indexing_indexer_info = indexers_info_table.get(&indexing_indexer_id)?;

                    // The indexer deployments list contains the healthy deployments. It must
                    // contain the deployment ID, otherwise, that means it was filtered out,
                    // e.g., invalid POI blocklist, etc.
                    if !indexing_indexer_info.deployments.contains(&deployment_id) {
                        return None;
                    }

                    let indexing_indexer = indexings_indexers_table.get(&indexing_indexer_id)?;

                    let indexing_largest_allocation_addr = indexing_indexer_info
                        .largest_allocation
                        .get(&deployment_id)?;

                    let indexing_total_allocated_tokens = indexing_indexer_info
                        .total_allocated_tokens
                        .get(&deployment_id)?;

                    let indexing_status = indexing_indexer_info
                        .indexings_status
                        .get(&deployment_id)
                        .map(|status| IndexingStatus {
                            latest_block: status.latest_block,
                            min_block: status.min_block,
                        });

                    let indexing_cost_model = indexing_indexer_info
                        .indexings_cost_models
                        .get(&deployment_id)
                        .cloned();

                    let indexing_id = IndexingId {
                        indexer: indexing_indexer_id,
                        deployment: deployment_id,
                    };
                    let indexing = Indexing {
                        id: indexing_id,
                        versions_behind: deployment_versions_behind,
                        largest_allocation: *indexing_largest_allocation_addr,
                        total_allocated_tokens: *indexing_total_allocated_tokens,
                        indexer: indexing_indexer.clone(),
                        status: indexing_status,
                        cost_model: indexing_cost_model,
                    };
                    Some((indexing_id, indexing))
                })
                .collect::<HashMap<_, _>>();
            if deployment_indexings.is_empty() {
                return None;
            }

            let deployment_subgraphs = subgraphs
                .iter()
                .filter_map(|(subgraph_id, subgraph)| {
                    if subgraph.deployments.contains(&deployment_id) {
                        Some(*subgraph_id)
                    } else {
                        None
                    }
                })
                .collect::<HashSet<_>>();
            if deployment_subgraphs.is_empty() {
                return None;
            }

            Some((
                deployment_id,
                Arc::new(Deployment {
                    id: deployment_id,
                    transferred_to_l2: deployment.transferred_to_l2,
                    chain: deployment_manifest_chain,
                    start_block: deployment_manifest_start_block,
                    subgraphs: deployment_subgraphs,
                    indexings: deployment_indexings,
                }),
            ))
        })
        .collect();

    GraphNetwork {
        deployments,
        subgraphs,
    }
}