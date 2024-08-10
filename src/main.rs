use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures::future::{self, BoxFuture};
use futures::stream::{self, select_all, BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt, TryStreamExt};
use indicatif::{ProgressState, ProgressStyle};
use nixapi::{check_cache_status, CacheStatus, Derivation, DrvPath, OutputName, StorePath};
use petgraph::algo::toposort;
use petgraph::visit::{EdgeRef as _, IntoNodeReferences};
use petgraph::Direction::{Incoming, Outgoing};
use petgraph::{Directed, Graph};
use reqwest::Url;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::select;
use tokio::sync::{OnceCell, Semaphore};
use tracing::level_filters::LevelFilter;
use tracing::{info, info_span, warn, Level};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::Layer;

mod nixapi;

#[derive(Debug, Parser)]
struct Args {
    attr: String,
    #[arg(long)]
    up_to: Option<String>,
    #[arg(long)]
    out_dir: Option<PathBuf>,
    #[arg(long, default_value = "./gc-roots")]
    gc_roots_dir: PathBuf,
    #[arg(long, default_value_t = 1)]
    max_jobs: usize,
    #[arg(long, default_value_t = 16)]
    max_fetch_jobs: usize,
    #[arg(long)]
    upload_command: Option<String>,
    #[arg(long, short, default_values = &["https://cache.nixos.org"])]
    substituters: Vec<Url>,
}

fn init_logging() {
    let indicatif_layer = IndicatifLayer::new().with_max_progress_bars(16, None);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(indicatif_layer.get_stderr_writer())
                .with_filter(LevelFilter::from_level(Level::INFO)),
        )
        .with(indicatif_layer)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let args = Args::parse();

    let mut drvs = BTreeMap::new();
    let leaf_drvs = evaluate(&args.attr, &args.gc_roots_dir).await?;
    if let Some(up_to) = &args.up_to {
        let (up_to_drv_path, _) = leaf_drvs
            .iter()
            .find(|(_drv_path, drv)| drv.attr.as_ref() == Some(up_to))
            .with_context(|| {
                format!(
                    "No derivation with attribute '{}' found in flake '{}'",
                    up_to, args.attr
                )
            })?;
        collect_drv_closure(&mut drvs, &leaf_drvs[up_to_drv_path]).await?;
    } else {
        for drv in leaf_drvs.values() {
            collect_drv_closure(&mut drvs, drv).await?;
        }
    }
    let drvs = Arc::new(drvs);

    let substituters = [Url::parse("https://cache.nixos.org")?];
    let cache_checker = CacheStatusChecker::new(substituters.to_vec(), 16);
    let cache_statuses = cache_checker
        .clone()
        .check_drvs(&drvs.values().cloned().collect::<Vec<_>>())
        .await;

    let to_build: HashMap<DrvPath, Derivation> = drvs
        .iter()
        .filter(|(drv_path, _drv)| cache_statuses[*drv_path] == CacheStatus::NotBuilt)
        .map(|(drv_path, drv)| (drv_path.clone(), drv.clone()))
        .collect();

    let mut graph = Graph::<DrvPath, (), Directed>::new();
    let mut drv_nodes = BTreeMap::new();
    for drv_path in to_build.keys() {
        let node = graph.add_node(drv_path.clone());
        drv_nodes.insert(drv_path, node);
    }
    for (drv_path, drv) in to_build.iter() {
        let drv_node = drv_nodes.get(&drv_path).unwrap();
        for (input, _) in &drv.inputs {
            if let Some(input_node) = drv_nodes.get(input) {
                graph.add_edge(*drv_node, *input_node, ());
            }
        }
    }

    let cache_uploader = args.upload_command.as_ref().map(|command| {
        Arc::new(CacheUploader::new(
            cache_checker.clone(),
            command.clone(),
            16,
        ))
    });

    let local_drvs_stream = stream::iter(drvs.values())
        .filter(|drv| async { cache_checker.clone().check_drv(drv).await == CacheStatus::Local })
        .then(|drv| future::ready(anyhow::Ok(drv.clone())))
        .boxed();

    let build_stream = build_drvs(
        &graph,
        &drvs,
        args.out_dir.as_deref(),
        args.max_jobs,
        args.max_fetch_jobs,
    );

    let drvs_clone = drvs.clone();
    let cache_checker_clone = cache_checker.clone();
    let built_drvs_stream = build_stream
        .map(move |(drv_path, status)| match status {
            BuildStatus::Success => {
                let drv = drvs_clone.get(&drv_path).unwrap().clone();
                for output in drv.outputs.values() {
                    cache_checker_clone.mark_local(output);
                }
                anyhow::Ok(drv)
            }
            BuildStatus::Failure => {
                bail!("Build failed");
            }
        })
        .boxed();

    let cache_uploader_clone = cache_uploader.clone();
    select_all([built_drvs_stream, local_drvs_stream])
        .map_ok(move |drv| {
            let cache_uploader = cache_uploader_clone.clone();
            tokio::spawn(async move {
                if let Some(cache_uploader) = &cache_uploader {
                    if drv.attr.is_some() {
                        cache_uploader.upload_drv_outputs(&drv, true).await?;
                    }
                }
                Ok(())
            })
            .then(|result| future::ready(result.unwrap()))
        })
        .try_buffer_unordered(16)
        .for_each(|_| async {})
        .await;

    // let components = connected_components(&graph);
    // let subgraphs = create_subgraphs(&graph, &components);
    // println!("To build: {}", graph.node_count());
    // println!("Subgraphs: {}", subgraphs.len());
    // fs::create_dir_all("subgraphs")?;
    // for (i, subgraph) in subgraphs.iter().enumerate() {
    //     println!("  {i}: node_count = {}", subgraph.node_count());
    //     let mut dot_file = fs::File::create(format!("subgraphs/{i}.dot"))?;
    //     let content = format!("{:?}", Dot::new(&subgraph));
    //     dot_file.write_all(content.as_bytes())?;
    // }

    Ok(())
}

enum BuildStatus {
    Success,
    Failure,
}

fn build_drvs<'a>(
    graph: &'a Graph<DrvPath, ()>,
    drvs: &'a BTreeMap<DrvPath, Derivation>,
    out_dir: Option<&'a Path>,
    max_jobs: usize,
    max_fetch_jobs: usize,
) -> BoxStream<'a, (DrvPath, BuildStatus)> {
    async_stream::stream! {
        let mut drv_dependencies: HashMap<&DrvPath, HashSet<&DrvPath>> = graph
            .node_references()
            .map(|(node, drv_path)| {
                let dependencies = graph
                    .edges_directed(node, Outgoing)
                    .map(|edge| &graph[edge.target()])
                    .collect();
                (drv_path, dependencies)
            })
            .collect();
        let drv_dependants: HashMap<&DrvPath, HashSet<&DrvPath>> = graph
            .node_references()
            .map(|(node, drv_path)| {
                let dependencies = graph
                    .edges_directed(node, Incoming)
                    .map(|edge| &graph[edge.source()])
                    .collect();
                (drv_path, dependencies)
            })
            .collect();
        let mut drv_dependants_recursive: HashMap<&DrvPath, HashSet<&DrvPath>> = HashMap::new();
        for node in toposort(&graph, None).unwrap() {
            let drv_path = &graph[node];
            let mut all_dependants = HashSet::new();
            for dependant in drv_dependants[drv_path].iter().copied() {
                all_dependants.insert(dependant);
                all_dependants.extend(drv_dependants_recursive[dependant].iter());
            }
            drv_dependants_recursive.insert(drv_path, all_dependants);
        }

        let mut remaining_drv_paths: HashSet<_> = graph.node_weights().collect();

        let header_span = info_span!("header");
        header_span.pb_set_style(&ProgressStyle::default_bar());
        header_span.pb_set_length(remaining_drv_paths.len() as _);
        header_span.pb_start();

        let mut builds_in_progress: HashMap<DrvPath, BoxFuture<Result<()>>> = HashMap::new();
        let mut fetches_in_progress: HashMap<DrvPath, BoxFuture<Result<()>>> = HashMap::new();
        let mut success_count = 0;

        // Build loop
        loop {
            if remaining_drv_paths.is_empty() && builds_in_progress.is_empty() && fetches_in_progress.is_empty() {
                return;
            }

            let can_build = builds_in_progress.len() < max_jobs;
            let can_fetch = fetches_in_progress.len() < max_fetch_jobs;

            // We can build derications that have no dependencies
            let mut buildable_drvs: Vec<_> = remaining_drv_paths
                .iter()
                .copied()
                .filter(|drv_path| {
                    let is_source = drvs.get(drv_path).unwrap().is_source();
                    drv_dependencies[drv_path].is_empty() && ((can_fetch && is_source) || (can_build && !is_source))
                })
                .cloned()
                .collect();

            // Check if we can start a new build
            if !buildable_drvs.is_empty() {
                // Build the derivation with the most dependants first
                buildable_drvs.sort_by_cached_key(|drv_path| {
                    (drv_dependants_recursive[drv_path].len(), drv_path.clone())
                });
                let to_build = buildable_drvs.last().cloned().unwrap();
                info!(drv = %to_build.as_str(), "Building");
                let out_path = out_dir.map(|dir| {
                    let drv_name = to_build.file_stem().unwrap();
                    dir.join(drv_name)
                });

                let to_build_clone = to_build.clone();
                let task =
                    tokio::spawn(async move { build_drv(&to_build_clone, out_path.as_deref()).await })
                        .then(|result| future::ready(result.unwrap()))
                        .boxed();
                remaining_drv_paths.remove(&to_build);

                if drvs[&to_build].is_source() {
                    fetches_in_progress.insert(to_build, task);
                } else {
                    builds_in_progress.insert(to_build, task);
                }
                continue;
            }

            // Create a temporary stream and wait for the first completed build
            let (completed_drv_path, result) = FuturesUnordered::from_iter(
                builds_in_progress
                    .iter_mut().chain(fetches_in_progress.iter_mut())
                    .map(|(drv_path, task)| async move { (drv_path.clone(), task.await) }),
            )
            .next()
            .await
            .unwrap();

            if drvs.get(&completed_drv_path).unwrap().is_source() {
                fetches_in_progress.remove(&completed_drv_path);
            } else {
                builds_in_progress.remove(&completed_drv_path);
            }
            match result {
                Ok(_) => {
                    info!(drv = %completed_drv_path.as_str(), "Build succeeded");
                    for dependencies in drv_dependencies.values_mut() {
                        dependencies.remove(&completed_drv_path);
                    }
                    yield (completed_drv_path, BuildStatus::Success);
                    header_span.pb_inc(1);
                    success_count += 1;
                }
                Err(_) => {
                    warn!(drv = %completed_drv_path.as_str(), "Build failed");
                    for dependant in drv_dependants_recursive[&completed_drv_path].iter() {
                        remaining_drv_paths.remove(dependant);
                    }
                    yield (completed_drv_path, BuildStatus::Failure);
                    header_span.pb_set_length((remaining_drv_paths.len() + success_count) as _);
                }
            }
        }
    }
    .boxed()
}

async fn build_drv(drv_path: &DrvPath, out_path: Option<&Path>) -> Result<()> {
    let mut cmd = Command::new("nix-build");
    cmd.kill_on_drop(true);
    if let Some(out_path) = out_path {
        cmd.arg("--out-link").arg(out_path);
    } else {
        cmd.arg("--no-out-link");
    }
    cmd.arg(drv_path.as_str());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());

    let drv_name = drv_path.get_name().to_string();
    let header_span = info_span!("header");
    header_span.pb_set_style(
        &ProgressStyle::with_template(
            "{spinner} {elapsed} 🔨Building {name:.cyan}: {wide_msg:.green}",
        )
        .unwrap()
        .with_key(
            "name",
            move |_state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                let _ = write!(writer, "{}", drv_name);
            },
        ),
    );
    header_span.pb_start();

    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take().unwrap();
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        info!("{}", line);
        header_span.pb_set_message(&line);
    }
    let status = child.wait().await?;
    if !status.success() {
        bail!("Failed to build '{}'", drv_path.as_str());
    }
    Ok(())
}

struct CacheStatusChecker {
    substituters: Vec<Url>,
    client: reqwest::Client,
    cache: Mutex<HashMap<StorePath, Arc<OnceCell<CacheStatus>>>>,
    limiter: Semaphore,
    max_jobs: usize,
}

impl CacheStatusChecker {
    fn new(substituters: Vec<Url>, max_jobs: usize) -> Arc<Self> {
        Arc::new(Self {
            substituters,
            client: reqwest::Client::new(),
            cache: Mutex::new(HashMap::new()),
            limiter: Semaphore::new(max_jobs),
            max_jobs,
        })
    }

    async fn check_path(&self, path: &StorePath) -> CacheStatus {
        let cell = {
            let mut cache = self.cache.lock().unwrap();
            cache
                .entry(path.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        *cell
            .get_or_init(|| async {
                let _permit = self.limiter.acquire().await;
                check_cache_status(&self.client, path, &self.substituters).await
            })
            .await
    }

    async fn check_drv(&self, drv: &Derivation) -> CacheStatus {
        self.check_path(&drv.outputs[&OutputName("out".to_string())])
            .await
    }

    async fn check_drvs(self: Arc<Self>, drvs: &[Derivation]) -> HashMap<DrvPath, CacheStatus> {
        info!("Checking cache status");
        let span = info_span!("header").entered();
        span.pb_set_style(&ProgressStyle::default_bar());
        span.pb_set_length(drvs.len() as _);
        stream::iter(drvs.iter().cloned())
            .map(|drv| {
                let span = span.clone();
                let self_clone = self.clone();
                tokio::spawn(async move {
                    let status = self_clone.check_drv(&drv).await;
                    span.pb_inc(1);
                    (drv.path.clone(), status)
                })
            })
            .buffer_unordered(self.max_jobs)
            .map(|result| {
                let (drv_path, status) = result.unwrap();
                (drv_path, status)
            })
            .collect()
            .await
    }

    fn mark_cached(&self, path: &StorePath) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(
            path.clone(),
            Arc::new(OnceCell::new_with(Some(CacheStatus::Cached))),
        );
    }

    fn mark_local(&self, path: &StorePath) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(
            path.clone(),
            Arc::new(OnceCell::new_with(Some(CacheStatus::Local))),
        );
    }
}

struct CacheUploader {
    cache_checker: Arc<CacheStatusChecker>,
    upload_command: String,
    limiter: Semaphore,
}

impl CacheUploader {
    fn new(
        cache_checker: Arc<CacheStatusChecker>,
        upload_command: String,
        max_jobs: usize,
    ) -> Self {
        Self {
            cache_checker,
            upload_command,
            limiter: Semaphore::new(max_jobs),
        }
    }

    async fn upload_path(&self, path: &StorePath) -> Result<()> {
        if self.cache_checker.check_path(path).await == CacheStatus::Cached {
            return Ok(());
        }
        let _permit = self.limiter.acquire().await;
        info!(path = %path.as_str(), "Uploading");
        let header_span = info_span!("header");
        let path_clone = path.clone();
        header_span.pb_set_style(
            &ProgressStyle::with_template(
                "{spinner} {elapsed} ⬆ Uploading {name:.orange}: {wide_msg:.green}",
            )
            .unwrap()
            .with_key(
                "name",
                move |_state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                    let _ = write!(writer, "{}", path_clone.get_name());
                },
            ),
        );
        header_span.pb_start();
        let mut child = Command::new("bash")
            .kill_on_drop(true)
            .arg("-c")
            .arg(&self.upload_command)
            .env("UPLOAD_STORE_PATH", path.as_str())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        loop {
            let line = select! {
                line = stdout_lines.next_line() => line,
                line = stderr_lines.next_line() => line,
            };
            match line {
                Ok(Some(line)) => {
                    info!("{}", line);
                    header_span.pb_set_message(&line);
                }
                Ok(None) => break,
                Err(e) => bail!(e),
            }
        }
        self.cache_checker.mark_cached(path);
        info!(path = %path.as_str(), "Upload succeeded");
        Ok(())
    }

    async fn upload_drv_outputs(&self, drv: &Derivation, skip_fod: bool) -> Result<()> {
        if skip_fod && drv.env.contains_key("outputHash") {
            return Ok(());
        }
        if let Some(allow_substitutes) = drv.env.get("allowSubstitutes") {
            if allow_substitutes != "1" {
                return Ok(());
            }
        }
        for output in drv.outputs.values() {
            if output.as_ref().exists() {
                self.upload_path(output).await?;
            }
        }
        Ok(())
    }
}

// fn create_subgraphs(
//     g: &Graph<DrvPath, ()>,
//     components: &[Vec<NodeIndex>],
// ) -> Vec<Graph<DrvPath, ()>> {
//     let mut subgraphs = Vec::new();
//     for component in components {
//         let mut subgraph = Graph::new();
//         let mut node_map = HashMap::new();
//         for node in component {
//             let new_node = subgraph.add_node(g[*node].clone());
//             node_map.insert(*node, new_node);
//         }
//         for edge in g.edge_references() {
//             if let (Some(source), Some(target)) =
//                 (node_map.get(&edge.source()), node_map.get(&edge.target()))
//             {
//                 subgraph.add_edge(*source, *target, ());
//             }
//         }
//         subgraphs.push(subgraph);
//     }
//     subgraphs
// }
//
// fn connected_components<G>(g: G) -> Vec<Vec<G::NodeId>>
// where
//     G: NodeCompactIndexable + IntoEdgeReferences,
// {
//     let mut vertex_sets = UnionFind::new(g.node_bound());
//     for edge in g.edge_references() {
//         let (a, b) = (edge.source(), edge.target());
//
//         // union the two vertices of the edge
//         vertex_sets.union(g.to_index(a), g.to_index(b));
//     }
//     let labels = vertex_sets.into_labeling();
//     let mut label_set = BTreeMap::new();
//     for label in labels.iter().copied() {
//         let len = label_set.len();
//         label_set.entry(label).or_insert(len);
//     }
//     let mut components = vec![Vec::new(); label_set.len()];
//     for (node_index, label) in labels.iter().enumerate() {
//         let node = g.from_index(node_index);
//         components[label_set[label]].push(node);
//     }
//     components
// }

#[async_recursion::async_recursion]
async fn collect_drv_closure(
    result: &mut BTreeMap<DrvPath, Derivation>,
    drv: &Derivation,
) -> Result<()> {
    if result.contains_key(&drv.path) {
        return Ok(());
    }
    result.insert(drv.path.clone(), drv.clone());
    for (input, _) in &drv.inputs {
        if result.contains_key(input) {
            continue;
        }
        let input_drv = Derivation::parse(input.clone())?;
        collect_drv_closure(result, &input_drv).await?;
    }
    Ok(())
}

async fn evaluate(attr: &str, gc_roots_dir: &Path) -> Result<BTreeMap<DrvPath, Derivation>> {
    info!(attr = %attr, "Evaluating flake");
    let header_span = info_span!("header");
    header_span.pb_set_style(&ProgressStyle::default_spinner());
    header_span.pb_set_message(&format!("Evaluating {attr}"));
    header_span.pb_start();

    #[derive(Debug, Deserialize)]
    struct Line {
        attr: String,
        #[serde(rename = "attrPath")]
        _attr_path: Vec<String>,
        error: Option<String>,
        #[serde(rename = "drvPath")]
        drv_path: Option<DrvPath>,
    }

    let mut child = Command::new("nix-eval-jobs")
        .arg("--flake")
        .arg(attr)
        .arg("--gc-roots-dir")
        .arg(gc_roots_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut results = BTreeMap::new();
    while let Some(line) = lines.next_line().await? {
        let parsed = serde_json::from_str::<Line>(&line)?;
        if let Some(error) = parsed.error {
            warn!(attr = %parsed.attr, error = %error, "evaluation failed");
        } else {
            let mut drv = Derivation::parse(parsed.drv_path.clone().unwrap())?;
            drv.attr = Some(parsed.attr);
            results.insert(parsed.drv_path.clone().unwrap(), drv);
        }
    }

    info!(attr = %attr, "Evaluation complete");
    Ok(results)
}
