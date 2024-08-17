use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use futures::future::{self, BoxFuture};
use futures::stream::{self, FuturesUnordered};
use futures::{FutureExt, Stream, StreamExt, TryStreamExt};
use fxhash::{FxHashMap, FxHashSet};
use indicatif::{ProgressState, ProgressStyle};
use nixapi::{check_cache_status, CacheStatus, Derivation, DrvPath, OutputName, StorePath};
use reqwest::Url;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::select;
use tokio::sync::{OnceCell, Semaphore};
use tracing::level_filters::LevelFilter;
use tracing::{error, info, info_span, warn, Level};
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
    up_to: Vec<String>,
    #[arg(long)]
    out_dir: Option<PathBuf>,
    #[arg(long, default_value = "./gc-roots")]
    gc_roots_dir: PathBuf,
    #[arg(long, default_value_t = 1)]
    max_jobs: usize,
    #[arg(long, default_value_t = 16)]
    max_local_jobs: usize,
    #[arg(long)]
    upload_command: Option<String>,
    #[arg(long, short, default_values = &["https://cache.nixos.org"])]
    substituters: Vec<Url>,
    #[arg(long, default_value_t = false)]
    keep_going: bool,
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

    let drv_by_attr = evaluate(&args.attr, &args.gc_roots_dir).await?;
    let attr_by_drv = Arc::new(
        drv_by_attr
            .iter()
            .map(|(attr, drv)| (drv.path.clone(), attr.clone()))
            .collect::<HashMap<_, _>>(),
    );

    let up_to_drvs: Vec<_> = if args.up_to.is_empty() {
        drv_by_attr.values().collect()
    } else {
        args.up_to
            .iter()
            .map(|attr| {
                drv_by_attr
                    .get(attr)
                    .with_context(|| format!("attribute '{}' not found", attr))
            })
            .collect::<Result<_>>()?
    };

    let mut drvs = HashMap::new();
    for drv in up_to_drvs.iter() {
        collect_drv_closure(&mut drvs, drv).await?;
    }
    let drvs = Arc::new(drvs);

    let mut graph = DrvGraph::new(drvs.values().cloned());

    let cache_checker = CacheStatusChecker::new(args.substituters.to_vec(), 16);
    let cache_statuses = cache_checker
        .clone()
        .check_drvs(&drvs.values().cloned().collect::<Vec<_>>())
        .await;

    drvs.keys()
        .filter(|drv_path| cache_statuses[*drv_path] != CacheStatus::NotBuilt)
        .for_each(|drv_path| {
            graph.remove(drv_path);
        });

    let should_available_drv_paths: Vec<&DrvPath> = drv_by_attr
        .values()
        .map(|drv| &drv.path)
        .filter(|drv_path| {
            up_to_drvs
                .iter()
                .any(|drv| graph.depends_on(&drv.path, drv_path))
        })
        .filter(|drv_path| cache_statuses[*drv_path] == CacheStatus::NotBuilt)
        .collect();

    let not_to_build_drv_paths: Vec<DrvPath> = graph
        .drv_paths()
        .filter(|drv_path| {
            should_available_drv_paths
                .iter()
                .all(|available_drv_path| !graph.depends_on(available_drv_path, drv_path))
        })
        .cloned()
        .collect();

    for drv_path in not_to_build_drv_paths {
        graph.remove(&drv_path);
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
        .then(|drv| future::ready(anyhow::Ok(drv.clone())));

    let final_build_graph_len = graph.len();

    let build_stream = build_drvs(
        graph,
        args.out_dir.clone(),
        args.max_jobs,
        args.max_local_jobs,
        args.keep_going,
    );

    let build_statuses = Arc::new(Mutex::new(HashMap::new()));
    let build_statuses_clone = build_statuses.clone();
    let drvs_clone = drvs.clone();
    let cache_checker_clone = cache_checker.clone();
    let built_drvs_stream = build_stream.filter_map(move |(drv_path, status)| {
        build_statuses_clone
            .lock()
            .unwrap()
            .insert(drv_path.clone(), status);
        match status {
            BuildStatus::Success => {
                let drv = drvs_clone.get(&drv_path).unwrap().clone();
                for output in drv.outputs.values() {
                    cache_checker_clone.mark_local(output);
                }
                future::ready(Some(anyhow::Ok(drv)))
            }
            BuildStatus::Failure => {
                if args.keep_going {
                    future::ready(None)
                } else {
                    error!("Aborting due to build error");
                    future::ready(Some(Err(anyhow!("Failed to build {}", drv_path.as_str()))))
                }
            }
        }
    });

    let cache_uploader_clone = cache_uploader.clone();
    let result = stream::select(built_drvs_stream, local_drvs_stream)
        .map_ok(move |drv| {
            let cache_uploader = cache_uploader_clone.clone();
            let attr_by_drv = attr_by_drv.clone();
            tokio::spawn(async move {
                if let Some(cache_uploader) = &cache_uploader {
                    if attr_by_drv.contains_key(&drv.path) {
                        cache_uploader.upload_drv_outputs(&drv, true).await?;
                    }
                }
                Ok(())
            })
            .map(|result| result.unwrap())
        })
        .try_buffer_unordered(16)
        .try_for_each(|_| future::ok(()))
        .await;

    let statuses = build_statuses.lock().unwrap();
    let total = final_build_graph_len;
    let built_count = statuses
        .values()
        .filter(|s| **s == BuildStatus::Success)
        .count();
    let failed_count = statuses
        .values()
        .filter(|s| **s == BuildStatus::Failure)
        .count();
    let canceled_count = final_build_graph_len - built_count - failed_count;
    match result {
        Ok(_) => {
            info!(total = %total, built = %built_count, failed = %failed_count, canceled = %canceled_count, "Done");
            Ok(())
        }
        Err(e) => {
            error!(total = %total, built = %built_count, failed = %failed_count, canceled = %canceled_count, "Failed to build some derivations");
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
struct DrvGraph {
    drv_paths: Vec<DrvPath>,
    idx_by_drv_path: FxHashMap<DrvPath, u32>,
    drvs: Vec<Derivation>,
    drv_dependencies: Vec<FxHashSet<u32>>,
    drv_dependants: Vec<FxHashSet<u32>>,
    drv_recursive_dependencies: Vec<FxHashSet<u32>>,
    drv_recursive_dependants: Vec<FxHashSet<u32>>,
}

impl DrvGraph {
    fn new(drvs: impl IntoIterator<Item = Derivation>) -> Self {
        let drvs: HashMap<DrvPath, Derivation> = drvs
            .into_iter()
            .map(|drv| (drv.path.clone(), drv.clone()))
            .collect();
        let all_drv_paths = 0..drvs.len();
        let drv_paths: Vec<_> = drvs.keys().cloned().collect();
        let idx_by_drv_path: FxHashMap<DrvPath, u32> = drv_paths
            .iter()
            .enumerate()
            .map(|(idx, drv_path)| (drv_path.clone(), idx as u32))
            .collect();
        let drvs: Vec<_> = drv_paths
            .iter()
            .map(|drv_path| drvs[drv_path].clone())
            .collect();

        let mut drv_dependencies = vec![FxHashSet::default(); drv_paths.len()];
        let mut drv_dependants = vec![FxHashSet::default(); drv_paths.len()];

        for (drv_path, drv) in drvs.iter().enumerate() {
            let dependencies = drv
                .inputs
                .iter()
                .map(|(input, _)| idx_by_drv_path[input])
                .collect();
            drv_dependencies[drv_path] = dependencies;
        }
        for dependant_drv_path in all_drv_paths {
            for dependency_drv_path in drv_dependencies[dependant_drv_path].iter() {
                drv_dependants[*dependency_drv_path as usize].insert(dependant_drv_path as u32);
            }
        }

        fn dfs(
            drv_path: u32,
            drv_neighbours: &Vec<FxHashSet<u32>>,
            result: &mut Vec<FxHashSet<u32>>,
            visited: &mut FxHashSet<u32>,
        ) {
            if visited.contains(&drv_path) {
                return;
            }
            visited.insert(drv_path);
            let mut rec_neighbours = FxHashSet::default();
            for neighbour_drv_path in drv_neighbours[drv_path as usize].iter().copied() {
                rec_neighbours.insert(neighbour_drv_path);
                dfs(neighbour_drv_path, drv_neighbours, result, visited);
                rec_neighbours.extend(result[neighbour_drv_path as usize].iter().cloned());
            }
            result[drv_path as usize] = rec_neighbours;
        }

        let mut rec_dependencies = vec![FxHashSet::default(); drv_paths.len()];
        let mut rec_dependants = vec![FxHashSet::default(); drv_paths.len()];
        let mut rec_dependencies_visited = FxHashSet::default();
        let mut rec_dependants_visited = FxHashSet::default();
        for drv_path in 0..drv_paths.len() {
            dfs(
                drv_path as _,
                &drv_dependencies,
                &mut rec_dependencies,
                &mut rec_dependencies_visited,
            );
            dfs(
                drv_path as _,
                &drv_dependants,
                &mut rec_dependants,
                &mut rec_dependants_visited,
            );
        }

        Self {
            drv_paths,
            idx_by_drv_path,
            drvs,
            drv_dependencies,
            drv_dependants,
            drv_recursive_dependencies: rec_dependencies,
            drv_recursive_dependants: rec_dependants,
        }
    }

    fn remove(&mut self, drv_path: &DrvPath) {
        let idx = self.idx_by_drv_path[drv_path];
        let dependants = self.drv_dependants[idx as usize].clone();
        let dependencies = self.drv_dependencies[idx as usize].clone();
        let rec_dependants = self.drv_recursive_dependants[idx as usize].clone();
        let rec_dependencies = self.drv_recursive_dependencies[idx as usize].clone();
        self.idx_by_drv_path.remove(drv_path);
        for dependant in dependants {
            self.drv_dependencies[dependant as usize].remove(&idx);
        }
        for dependency in dependencies {
            self.drv_dependants[dependency as usize].remove(&idx);
        }
        for dependant in rec_dependants {
            self.drv_recursive_dependencies[dependant as usize].remove(&idx);
        }
        for dependency in rec_dependencies {
            self.drv_recursive_dependants[dependency as usize].remove(&idx);
        }
    }

    fn remove_recursive_dependants(&mut self, drv_path: &DrvPath) {
        let drv_path = self.idx_by_drv_path[drv_path];
        for dependant in self.drv_recursive_dependants[drv_path as usize].clone() {
            let p = self.drv_paths[dependant as usize].clone();
            self.remove(&p);
        }
    }

    fn len(&self) -> usize {
        self.idx_by_drv_path.len()
    }

    fn drv_paths(&self) -> impl Iterator<Item = &DrvPath> {
        self.idx_by_drv_path.keys()
    }

    fn drv(&self, drv_path: &DrvPath) -> &Derivation {
        &self.drvs[self.idx_by_drv_path[drv_path] as usize]
    }

    fn has_no_dependencies(&self, drv_path: &DrvPath) -> bool {
        self.drv_dependencies[self.idx_by_drv_path[drv_path] as usize].is_empty()
    }

    fn recursive_dependant_count(&self, drv_path: &DrvPath) -> usize {
        self.drv_recursive_dependants[self.idx_by_drv_path[drv_path] as usize].len()
    }

    fn depends_on(&self, drv_path: &DrvPath, other_drv_path: &DrvPath) -> bool {
        let Some(drv_path) = self.idx_by_drv_path.get(drv_path) else {
            return false;
        };
        let Some(other_drv_path) = self.idx_by_drv_path.get(other_drv_path) else {
            return false;
        };
        self.drv_recursive_dependencies[*drv_path as usize].contains(other_drv_path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildStatus {
    Success,
    Failure,
}

fn build_drvs(
    mut graph: DrvGraph,
    out_dir: Option<PathBuf>,
    max_jobs: usize,
    max_local_jobs: usize,
    keep_going: bool,
) -> impl Stream<Item = (DrvPath, BuildStatus)> {
    async_fn_stream::fn_stream(move |emitter| async move {
        let mut remaining_drv_paths: HashSet<_> = graph.drv_paths().cloned().collect();

        let header_span = info_span!("header");
        header_span.pb_set_style(&ProgressStyle::default_bar());
        header_span.pb_set_length(remaining_drv_paths.len() as _);
        header_span.pb_start();

        let mut builds_in_progress: HashMap<DrvPath, BoxFuture<Result<()>>> = HashMap::new();
        let mut local_builds_in_progress: HashMap<DrvPath, BoxFuture<Result<()>>> = HashMap::new();
        let mut success_count = 0;

        // Build loop
        loop {
            if remaining_drv_paths.is_empty()
                && builds_in_progress.is_empty()
                && local_builds_in_progress.is_empty()
            {
                return;
            }

            let can_build = builds_in_progress.len() < max_jobs;
            let can_build_local = local_builds_in_progress.len() < max_local_jobs;

            // We can build derications that have no dependencies
            let mut buildable_drvs: Vec<_> = remaining_drv_paths
                .iter()
                .filter(|drv_path| {
                    let is_local = graph.drv(drv_path).is_local();
                    graph.has_no_dependencies(drv_path)
                        && ((can_build_local && is_local) || (can_build && !is_local))
                })
                .cloned()
                .collect();

            // Check if we can start a new build
            if !buildable_drvs.is_empty() {
                // Build the derivation with the most dependants first
                buildable_drvs.sort_by_cached_key(|drv_path| {
                    (graph.recursive_dependant_count(drv_path), drv_path.clone())
                });
                let to_build = buildable_drvs.last().cloned().unwrap();
                info!(drv = %to_build.as_str(), "Building");
                let out_path = out_dir.as_ref().map(|dir| {
                    let drv_name = to_build.file_stem().unwrap();
                    dir.join(drv_name)
                });

                let to_build_clone = to_build.clone();
                let task =
                    tokio::spawn(
                        async move { build_drv(&to_build_clone, out_path.as_deref()).await },
                    )
                    .map(|result| result.unwrap())
                    .boxed();
                remaining_drv_paths.remove(&to_build);

                if graph.drv(&to_build).is_local() {
                    local_builds_in_progress.insert(to_build, task);
                } else {
                    builds_in_progress.insert(to_build, task);
                }
                continue;
            }

            // Create a temporary stream and wait for the first completed build
            let (completed_drv_path, result) = FuturesUnordered::from_iter(
                builds_in_progress
                    .iter_mut()
                    .chain(local_builds_in_progress.iter_mut())
                    .map(|(drv_path, task)| async move { (drv_path.clone(), task.await) }),
            )
            .next()
            .await
            .unwrap();

            if graph.drv(&completed_drv_path).is_local() {
                local_builds_in_progress.remove(&completed_drv_path);
            } else {
                builds_in_progress.remove(&completed_drv_path);
            }
            match result {
                Ok(_) => {
                    info!(name = %completed_drv_path.get_name(), drv = %completed_drv_path.as_str(), "Build succeeded");
                    graph.remove(&completed_drv_path);
                    emitter
                        .emit((completed_drv_path, BuildStatus::Success))
                        .await;
                    header_span.pb_inc(1);
                    success_count += 1;
                }
                Err(_) => {
                    if keep_going {
                        warn!(name = %completed_drv_path.get_name(), drv = %completed_drv_path.as_str(), "Build failed");
                    } else {
                        error!(name = %completed_drv_path.get_name(), drv = %completed_drv_path.as_str(), "Build failed");
                        emitter
                            .emit((completed_drv_path, BuildStatus::Failure))
                            .await;
                        return;
                    }
                    graph.remove_recursive_dependants(&completed_drv_path);
                    emitter
                        .emit((completed_drv_path, BuildStatus::Failure))
                        .await;
                    header_span.pb_set_length((remaining_drv_paths.len() + success_count) as _);
                }
            }
        }
    })
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

#[async_recursion::async_recursion]
async fn collect_drv_closure(
    result: &mut HashMap<DrvPath, Derivation>,
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

async fn evaluate(attr: &str, gc_roots_dir: &Path) -> Result<BTreeMap<String, Derivation>> {
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
            let drv = Derivation::parse(parsed.drv_path.clone().unwrap())?;
            results.insert(parsed.attr, drv);
        }
    }

    info!(attr = %attr, "Evaluation complete");
    Ok(results)
}
