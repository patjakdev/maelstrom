pub mod artifacts;
pub mod cargo;
pub mod config;
pub mod metadata;
pub mod pattern;
pub mod progress;
pub mod test_listing;
pub mod visitor;

#[cfg(test)]
mod tests;

use anyhow::{anyhow, Result};
use artifacts::GeneratedArtifacts;
use cargo::{CompilationOptions, FeatureSelectionOptions, ManifestOptions};
use cargo_metadata::{Artifact as CargoArtifact, Package as CargoPackage, PackageId};
use config::Quiet;
use indicatif::TermLike;
use maelstrom_base::{
    stats::JobStateCounts, ArtifactType, ClientJobId, JobOutcomeResult, JobSpec, NonEmpty,
    Sha256Digest, Timeout,
};
use maelstrom_client::{
    spec::{ImageConfig, Layer},
    ArtifactUploadProgress, Client, ClientBgProcess,
};
use maelstrom_util::{
    config::common::{BrokerAddr, CacheSize, InlineLimit, LogLevel, Slots},
    process::ExitCode,
    template::TemplateVars,
};
use metadata::{AllMetadata, TestMetadata};
use progress::{
    MultipleProgressBars, NoBar, ProgressDriver, ProgressIndicator, QuietNoBar, QuietProgressBar,
    TestListingProgress, TestListingProgressNoSpinner,
};
use slog::Drain as _;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    path::{Path, PathBuf},
    str,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use test_listing::{load_test_listing, write_test_listing, TestListing, LAST_TEST_LISTING_NAME};
use visitor::{JobStatusTracker, JobStatusVisitor};

#[derive(Debug)]
pub enum ListAction {
    ListTests,
    ListBinaries,
    ListPackages,
}

/// Returns `true` if the given `CargoPackage` matches the given pattern
fn filter_package(package: &CargoPackage, p: &pattern::Pattern) -> bool {
    let c = pattern::Context {
        package: package.name.clone(),
        artifact: None,
        case: None,
    };
    pattern::interpret_pattern(p, &c).unwrap_or(true)
}

/// Returns `true` if the given `CargoArtifact` and case matches the given pattern
fn filter_case(
    package_name: &str,
    artifact: &CargoArtifact,
    case: &str,
    p: &pattern::Pattern,
) -> bool {
    let c = pattern::Context {
        package: package_name.into(),
        artifact: Some(pattern::Artifact::from_target(&artifact.target)),
        case: Some(pattern::Case { name: case.into() }),
    };
    pattern::interpret_pattern(p, &c).expect("case is provided")
}

fn do_template_replacement(
    test_metadata: &mut AllMetadata,
    compilation_options: &CompilationOptions,
    target_dir: &Path,
) -> Result<()> {
    let profile = compilation_options.profile.clone().unwrap_or("dev".into());
    let mut target = target_dir.to_owned();
    match profile.as_str() {
        "dev" => target.push("debug"),
        other => target.push(other),
    }
    let build_dir = target
        .to_str()
        .ok_or_else(|| anyhow!("{} contains non-UTF8", target.display()))?;
    let vars = TemplateVars::new()
        .with_var("build_dir", build_dir)
        .unwrap();
    test_metadata.replace_template_vars(&vars)?;
    Ok(())
}

/// A collection of objects that are used while enqueuing jobs. This is useful as a separate object
/// since it can contain things which live longer than the scoped threads and thus can be shared
/// among them.
///
/// This object is separate from `MainAppState` because it is lent to `JobQueuing`
struct JobQueuingState {
    packages: BTreeMap<PackageId, CargoPackage>,
    filter: pattern::Pattern,
    stderr_color: bool,
    tracker: Arc<JobStatusTracker>,
    jobs_queued: AtomicU64,
    test_metadata: AllMetadata,
    expected_job_count: u64,
    test_listing: Mutex<TestListing>,
    list_action: Option<ListAction>,
    feature_selection_options: FeatureSelectionOptions,
    compilation_options: CompilationOptions,
    manifest_options: ManifestOptions,
}

impl JobQueuingState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        packages: BTreeMap<PackageId, CargoPackage>,
        filter: pattern::Pattern,
        stderr_color: bool,
        mut test_metadata: AllMetadata,
        test_listing: TestListing,
        list_action: Option<ListAction>,
        target_directory: impl AsRef<Path>,
        feature_selection_options: FeatureSelectionOptions,
        compilation_options: CompilationOptions,
        manifest_options: ManifestOptions,
    ) -> Result<Self> {
        let expected_job_count = test_listing.expected_job_count(&filter);
        do_template_replacement(
            &mut test_metadata,
            &compilation_options,
            target_directory.as_ref(),
        )?;

        Ok(Self {
            packages,
            filter,
            stderr_color,
            tracker: Arc::new(JobStatusTracker::default()),
            jobs_queued: AtomicU64::new(0),
            test_metadata,
            expected_job_count,
            test_listing: Mutex::new(test_listing),
            list_action,
            feature_selection_options,
            compilation_options,
            manifest_options,
        })
    }
}

type StringIter = <Vec<String> as IntoIterator>::IntoIter;

/// Enqueues test cases as jobs in the given client from the given `CargoArtifact`
///
/// This object is like an iterator, it maintains a position in the test listing and enqueues the
/// next thing when asked.
///
/// This object is stored inside `JobQueuing` and is used to keep track of which artifact it is
/// currently enqueuing from.
struct ArtifactQueuing<'a, ProgressIndicatorT, MainAppDepsT> {
    log: slog::Logger,
    queuing_state: &'a JobQueuingState,
    deps: &'a MainAppDepsT,
    width: usize,
    ind: ProgressIndicatorT,
    artifact: CargoArtifact,
    binary: PathBuf,
    generated_artifacts: Option<GeneratedArtifacts>,
    ignored_cases: HashSet<String>,
    package_name: String,
    cases: StringIter,
    timeout_override: Option<Option<Timeout>>,
}

#[derive(Default)]
struct TestListingResult {
    cases: Vec<String>,
    ignored_cases: HashSet<String>,
}

fn list_test_cases(
    deps: &impl MainAppDeps,
    log: slog::Logger,
    queuing_state: &JobQueuingState,
    ind: &impl ProgressIndicator,
    artifact: &CargoArtifact,
    package_name: &str,
) -> Result<TestListingResult> {
    ind.update_enqueue_status(format!("getting test list for {package_name}"));

    slog::debug!(log, "listing ignored tests"; "binary" => ?artifact.executable);
    let binary = PathBuf::from(artifact.executable.clone().unwrap());
    let ignored_cases: HashSet<_> = deps
        .get_cases_from_binary(&binary, &Some("--ignored".into()))?
        .into_iter()
        .collect();

    slog::debug!(log, "listing tests"; "binary" => ?artifact.executable);
    let mut cases = deps.get_cases_from_binary(&binary, &None)?;

    let mut listing = queuing_state.test_listing.lock().unwrap();
    listing.add_cases(package_name, artifact, &cases[..]);

    cases.retain(|c| filter_case(package_name, artifact, c, &queuing_state.filter));
    Ok(TestListingResult {
        cases,
        ignored_cases,
    })
}

fn generate_artifacts(
    deps: &impl MainAppDeps,
    artifact: &CargoArtifact,
    log: slog::Logger,
) -> Result<GeneratedArtifacts> {
    let binary = PathBuf::from(artifact.executable.clone().unwrap());
    artifacts::add_generated_artifacts(deps, &binary, log)
}

impl<'a, ProgressIndicatorT, MainAppDepsT> ArtifactQueuing<'a, ProgressIndicatorT, MainAppDepsT>
where
    ProgressIndicatorT: ProgressIndicator,
    MainAppDepsT: MainAppDeps,
{
    #[allow(clippy::too_many_arguments)]
    fn new(
        log: slog::Logger,
        queuing_state: &'a JobQueuingState,
        deps: &'a MainAppDepsT,
        width: usize,
        ind: ProgressIndicatorT,
        artifact: CargoArtifact,
        package_name: String,
        timeout_override: Option<Option<Timeout>>,
    ) -> Result<Self> {
        let binary = PathBuf::from(artifact.executable.clone().unwrap());

        let running_tests = queuing_state.list_action.is_none();

        let listing = list_test_cases(
            deps,
            log.clone(),
            queuing_state,
            &ind,
            &artifact,
            &package_name,
        )?;

        ind.update_enqueue_status(format!("generating artifacts for {package_name}"));
        slog::debug!(
            log,
            "generating artifacts";
            "package_name" => &package_name,
            "artifact" => ?artifact);
        let generated_artifacts = running_tests
            .then(|| generate_artifacts(deps, &artifact, log.clone()))
            .transpose()?;

        Ok(Self {
            log,
            queuing_state,
            deps,
            width,
            ind,
            artifact,
            binary,
            generated_artifacts,
            ignored_cases: listing.ignored_cases,
            package_name,
            cases: listing.cases.into_iter(),
            timeout_override,
        })
    }

    fn calculate_job_layers(
        &mut self,
        test_metadata: &TestMetadata,
    ) -> Result<NonEmpty<(Sha256Digest, ArtifactType)>> {
        let mut layers = test_metadata
            .layers
            .iter()
            .map(|layer| {
                slog::debug!(self.log, "adding layer"; "layer" => ?layer);
                self.deps.add_layer(layer.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        let artifacts = self.generated_artifacts.as_ref().unwrap();
        if test_metadata.include_shared_libraries() {
            layers.push((artifacts.deps.clone(), ArtifactType::Manifest));
        }
        layers.push((artifacts.binary.clone(), ArtifactType::Manifest));

        Ok(NonEmpty::try_from(layers).unwrap())
    }

    fn format_case_str(&self, case: &str) -> String {
        let mut s = self.package_name.to_string();
        s += " ";

        let artifact_name = &self.artifact.target.name;
        if artifact_name != &self.package_name {
            s += artifact_name;
            s += " ";
        }
        s += case;
        s
    }

    fn queue_job_from_case(&mut self, case: &str) -> Result<EnqueueResult> {
        let case_str = self.format_case_str(case);
        self.ind
            .update_enqueue_status(format!("processing {case_str}"));
        slog::debug!(self.log, "enqueuing test case"; "case" => &case_str);

        if self.queuing_state.list_action.is_some() {
            self.ind.println(case_str);
            return Ok(EnqueueResult::Listed);
        }

        let image_lookup = |image: &str| {
            self.ind
                .update_enqueue_status(format!("downloading image {image}"));
            let (image, version) = image.split_once(':').unwrap_or((image, "latest"));
            slog::debug!(
                self.log, "getting container image";
                "image" => &image,
                "version" => &version,
            );
            self.deps.get_container_image(image, version)
        };

        let filter_context = pattern::Context {
            package: self.package_name.clone(),
            artifact: Some(pattern::Artifact::from_target(&self.artifact.target)),
            case: Some(pattern::Case { name: case.into() }),
        };

        let test_metadata = self
            .queuing_state
            .test_metadata
            .get_metadata_for_test_with_env(&filter_context, image_lookup)?;
        self.ind
            .update_enqueue_status(format!("calculating layers for {case_str}"));
        slog::debug!(&self.log, "calculating job layers"; "case" => &case_str);
        let layers = self.calculate_job_layers(&test_metadata)?;

        // N.B. Must do this before we enqueue the job, but after we know we can't fail
        let count = self
            .queuing_state
            .jobs_queued
            .fetch_add(1, Ordering::AcqRel);
        self.ind.update_length(std::cmp::max(
            self.queuing_state.expected_job_count,
            count + 1,
        ));

        let visitor = JobStatusVisitor::new(
            self.queuing_state.tracker.clone(),
            case_str.clone(),
            self.width,
            self.ind.clone(),
        );

        if self.ignored_cases.contains(case) {
            visitor.job_ignored();
            return Ok(EnqueueResult::Ignored);
        }

        self.ind
            .update_enqueue_status(format!("submitting job for {case_str}"));
        slog::debug!(&self.log, "submitting job"; "case" => &case_str);
        let binary_name = self.binary.file_name().unwrap().to_str().unwrap();
        self.deps.add_job(
            JobSpec {
                program: format!("/{binary_name}").into(),
                arguments: vec!["--exact".into(), "--nocapture".into(), case.into()],
                environment: test_metadata.environment(),
                layers,
                devices: test_metadata.devices,
                mounts: test_metadata.mounts,
                enable_loopback: test_metadata.enable_loopback,
                enable_writable_file_system: test_metadata.enable_writable_file_system,
                working_directory: test_metadata.working_directory,
                user: test_metadata.user,
                group: test_metadata.group,
                timeout: self.timeout_override.unwrap_or(test_metadata.timeout),
            },
            move |cjid, result| visitor.job_finished(cjid, result),
        )?;

        Ok(EnqueueResult::Enqueued {
            package_name: self.package_name.clone(),
            case: case.into(),
        })
    }

    /// Attempt to enqueue the next test as a job in the client
    ///
    /// Returns an `EnqueueResult` describing what happened. Meant to be called until it returns
    /// `EnqueueResult::Done`
    fn enqueue_one(&mut self) -> Result<EnqueueResult> {
        let Some(case) = self.cases.next() else {
            return Ok(EnqueueResult::Done);
        };
        self.queue_job_from_case(&case)
    }
}

/// Enqueues tests as jobs using the given deps.
///
/// This object is like an iterator, it maintains a position in the test listing and enqueues the
/// next thing when asked.
struct JobQueuing<'a, ProgressIndicatorT, MainAppDepsT: MainAppDeps> {
    log: slog::Logger,
    queuing_state: &'a JobQueuingState,
    deps: &'a MainAppDepsT,
    width: usize,
    ind: ProgressIndicatorT,
    wait_handle: Option<MainAppDepsT::CargoWaitHandle>,
    package_match: bool,
    artifacts: Option<MainAppDepsT::CargoTestArtifactStream>,
    artifact_queuing: Option<ArtifactQueuing<'a, ProgressIndicatorT, MainAppDepsT>>,
    timeout_override: Option<Option<Timeout>>,
}

impl<'a, ProgressIndicatorT: ProgressIndicator, MainAppDepsT>
    JobQueuing<'a, ProgressIndicatorT, MainAppDepsT>
where
    ProgressIndicatorT: ProgressIndicator,
    MainAppDepsT: MainAppDeps,
{
    fn new(
        log: slog::Logger,
        queuing_state: &'a JobQueuingState,
        deps: &'a MainAppDepsT,
        width: usize,
        ind: ProgressIndicatorT,
        timeout_override: Option<Option<Timeout>>,
    ) -> Result<Self> {
        let package_names: Vec<_> = queuing_state
            .packages
            .values()
            .map(|p| format!("{}@{}", &p.name, &p.version))
            .collect();

        let building_tests = !package_names.is_empty()
            && matches!(
                queuing_state.list_action,
                None | Some(ListAction::ListTests)
            );

        let (wait_handle, artifacts) = building_tests
            .then(|| {
                deps.run_cargo_test(
                    queuing_state.stderr_color,
                    &queuing_state.feature_selection_options,
                    &queuing_state.compilation_options,
                    &queuing_state.manifest_options,
                    package_names,
                )
            })
            .transpose()?
            .unzip();

        Ok(Self {
            log,
            queuing_state,
            deps,
            width,
            ind,
            package_match: false,
            artifacts,
            artifact_queuing: None,
            wait_handle,
            timeout_override,
        })
    }

    fn start_queuing_from_artifact(&mut self) -> Result<bool> {
        self.ind.update_enqueue_status("building artifacts...");

        slog::debug!(self.log, "getting artifact from cargo");
        let Some(ref mut artifacts) = self.artifacts else {
            return Ok(false);
        };
        let Some(artifact) = artifacts.next() else {
            return Ok(false);
        };
        let artifact = artifact?;

        slog::debug!(self.log, "got artifact"; "artifact" => ?artifact);
        let package_name = &self
            .queuing_state
            .packages
            .get(&artifact.package_id)
            .expect("artifact for unknown package")
            .name;

        self.artifact_queuing = Some(ArtifactQueuing::new(
            self.log.clone(),
            self.queuing_state,
            self.deps,
            self.width,
            self.ind.clone(),
            artifact,
            package_name.into(),
            self.timeout_override,
        )?);

        Ok(true)
    }

    /// Meant to be called when the user has enqueued all the jobs they want. Checks for deferred
    /// errors from cargo or otherwise
    fn finish(&mut self) -> Result<()> {
        slog::debug!(self.log, "checking for cargo errors");
        if let Some(wh) = self.wait_handle.take() {
            wh.wait()?;
        }
        Ok(())
    }

    /// Attempt to enqueue the next test as a job in the client
    ///
    /// Returns an `EnqueueResult` describing what happened. Meant to be called it returns
    /// `EnqueueResult::Done`
    fn enqueue_one(&mut self) -> Result<EnqueueResult> {
        slog::debug!(self.log, "enqueuing a job");

        if self.artifact_queuing.is_none() && !self.start_queuing_from_artifact()? {
            self.finish()?;
            return Ok(EnqueueResult::Done);
        }
        self.package_match = true;

        let res = self.artifact_queuing.as_mut().unwrap().enqueue_one()?;
        if res.is_done() {
            self.artifact_queuing = None;
            return self.enqueue_one();
        }

        Ok(res)
    }
}

pub trait Wait {
    fn wait(self) -> Result<()>;
}

impl Wait for cargo::WaitHandle {
    fn wait(self) -> Result<()> {
        cargo::WaitHandle::wait(self)
    }
}

pub trait MainAppDeps: Sync {
    fn add_layer(&self, layer: Layer) -> Result<(Sha256Digest, ArtifactType)>;

    fn get_artifact_upload_progress(&self) -> Result<Vec<ArtifactUploadProgress>>;

    fn get_job_state_counts(&self) -> Result<JobStateCounts>;

    fn get_container_image(&self, name: &str, tag: &str) -> Result<ImageConfig>;

    fn add_job(
        &self,
        spec: JobSpec,
        handler: impl FnOnce(ClientJobId, JobOutcomeResult) + Send + Sync + 'static,
    ) -> Result<()>;

    fn wait_for_outstanding_jobs(&self) -> Result<()>;

    type CargoWaitHandle: Wait;
    type CargoTestArtifactStream: Iterator<Item = Result<CargoArtifact>>;

    fn run_cargo_test(
        &self,
        color: bool,
        feature_selection_options: &FeatureSelectionOptions,
        compilation_options: &CompilationOptions,
        manifest_options: &ManifestOptions,
        packages: Vec<String>,
    ) -> Result<(Self::CargoWaitHandle, Self::CargoTestArtifactStream)>;

    fn get_cases_from_binary(&self, binary: &Path, filter: &Option<String>) -> Result<Vec<String>>;
}

pub struct DefaultMainAppDeps {
    client: Client,
}

impl DefaultMainAppDeps {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bg_proc: ClientBgProcess,
        cache_dir: &impl AsRef<Path>,
        workspace_root: &impl AsRef<Path>,
        broker_addr: Option<BrokerAddr>,
        cache_size: CacheSize,
        inline_limit: InlineLimit,
        slots: Slots,
        log: slog::Logger,
    ) -> Result<Self> {
        slog::debug!(
            log, "creating app dependencies";
            "broker_addr" => ?broker_addr,
            "cache_size" => ?cache_size,
            "inline_limit" => ?inline_limit,
            "slots" => ?slots,
        );
        let client = Client::new(
            bg_proc,
            broker_addr,
            workspace_root,
            cache_dir,
            cache_size,
            inline_limit,
            slots,
            log,
        )?;
        Ok(Self { client })
    }
}

impl MainAppDeps for DefaultMainAppDeps {
    fn add_layer(&self, layer: Layer) -> Result<(Sha256Digest, ArtifactType)> {
        self.client.add_layer(layer)
    }

    fn get_artifact_upload_progress(&self) -> Result<Vec<ArtifactUploadProgress>> {
        self.client.get_artifact_upload_progress()
    }

    fn get_job_state_counts(&self) -> Result<JobStateCounts> {
        self.client.get_job_state_counts()
    }

    fn get_container_image(&self, name: &str, tag: &str) -> Result<ImageConfig> {
        let image = self.client.get_container_image(name, tag)?;
        Ok(ImageConfig {
            layers: image.layers.clone(),
            environment: image.env().cloned(),
            working_directory: image.working_dir().map(From::from),
        })
    }

    fn add_job(
        &self,
        spec: JobSpec,
        handler: impl FnOnce(ClientJobId, JobOutcomeResult) + Send + Sync + 'static,
    ) -> Result<()> {
        self.client.add_job(spec, handler)
    }

    fn wait_for_outstanding_jobs(&self) -> Result<()> {
        self.client.wait_for_outstanding_jobs()
    }

    type CargoWaitHandle = cargo::WaitHandle;
    type CargoTestArtifactStream = cargo::TestArtifactStream;

    fn run_cargo_test(
        &self,
        color: bool,
        feature_selection_options: &FeatureSelectionOptions,
        compilation_options: &CompilationOptions,
        manifest_options: &ManifestOptions,
        packages: Vec<String>,
    ) -> Result<(cargo::WaitHandle, cargo::TestArtifactStream)> {
        cargo::run_cargo_test(
            color,
            feature_selection_options,
            compilation_options,
            manifest_options,
            packages,
        )
    }

    fn get_cases_from_binary(&self, binary: &Path, filter: &Option<String>) -> Result<Vec<String>> {
        cargo::get_cases_from_binary(binary, filter)
    }
}

/// A collection of objects that are used to run the MainApp. This is useful as a separate object
/// since it can contain things which live longer than scoped threads and thus shared among them.
pub struct MainAppState<MainAppDepsT> {
    deps: MainAppDepsT,
    queuing_state: JobQueuingState,
    cache_dir: PathBuf,
    logging_output: LoggingOutput,
    log: slog::Logger,
}

impl<MainAppDepsT> MainAppState<MainAppDepsT> {
    /// Creates a new `MainAppState`
    ///
    /// `bg_proc`: handle to background client process
    /// `cargo`: the command to run when invoking cargo
    /// `include_filter`: tests which match any of the patterns in this filter are run
    /// `exclude_filter`: tests which match any of the patterns in this filter are not run
    /// `list_action`: if some, tests aren't run, instead tests or other things are listed
    /// `stderr_color`: should terminal color codes be written to `stderr` or not
    /// `workspace_root`: the path to the root of the workspace
    /// `workspace_packages`: a listing of the packages in the workspace
    /// `broker_addr`: the network address of the broker which we connect to
    /// `client_driver`: an object which drives the background work of the `Client`
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        deps: MainAppDepsT,
        include_filter: Vec<String>,
        exclude_filter: Vec<String>,
        list_action: Option<ListAction>,
        stderr_color: bool,
        workspace_root: &impl AsRef<Path>,
        workspace_packages: &[&CargoPackage],
        cache_directory: &impl AsRef<Path>,
        target_directory: &impl AsRef<Path>,
        feature_selection_options: FeatureSelectionOptions,
        compilation_options: CompilationOptions,
        manifest_options: ManifestOptions,
        logging_output: LoggingOutput,
        log: slog::Logger,
    ) -> Result<Self> {
        slog::debug!(
            log, "creating app state";
            "include_filter" => ?include_filter,
            "exclude_filter" => ?exclude_filter,
            "list_action" => ?list_action,
        );

        let test_metadata = AllMetadata::load(log.clone(), workspace_root)?;
        let mut test_listing =
            load_test_listing(&cache_directory.as_ref().join(LAST_TEST_LISTING_NAME))?
                .unwrap_or_default();
        test_listing.retain_packages(workspace_packages);

        let filter = pattern::compile_filter(&include_filter, &exclude_filter)?;
        let selected_packages: BTreeMap<_, _> = workspace_packages
            .iter()
            .filter(|p| filter_package(p, &filter))
            .map(|&p| (p.id.clone(), p.clone()))
            .collect();

        slog::debug!(
            log, "filtered packages";
            "selected_packages" => ?Vec::from_iter(selected_packages.keys()),
        );

        Ok(Self {
            deps,
            queuing_state: JobQueuingState::new(
                selected_packages,
                filter,
                stderr_color,
                test_metadata,
                test_listing,
                list_action,
                target_directory,
                feature_selection_options,
                compilation_options,
                manifest_options,
            )?,
            cache_dir: cache_directory.as_ref().to_owned(),
            logging_output,
            log,
        })
    }
}

/// The `MainApp` enqueues tests as jobs. With each attempted job enqueued this object is returned
/// and describes what happened.
pub enum EnqueueResult {
    /// A job successfully enqueued with the following information
    Enqueued { package_name: String, case: String },
    /// No job was enqueued, instead the test that would have been enqueued has been ignored
    /// because it has been marked as `#[ignored]`
    Ignored,
    /// No job was enqueued, we have run out of tests to run
    Done,
    /// No job was enqueued, we listed the test case instead
    Listed,
}

impl EnqueueResult {
    /// Is this `EnqueueResult` the `Done` variant
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// Is this `EnqueueResult` the `Ignored` variant
    pub fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored)
    }
}

/// This is the public API for the MainApp
///
/// N.B. This API is a trait only for type-erasure purposes
pub trait MainApp {
    /// Enqueue one test as a job on the `Client`. This is meant to be called repeatedly until
    /// `EnqueueResult::Done` is returned, or an error is encountered.
    fn enqueue_one(&mut self) -> Result<EnqueueResult>;

    /// Indicates that we have finished enqueuing jobs and starts tearing things down
    fn drain(&mut self) -> Result<()>;

    /// Waits for all outstanding jobs to finish, displays a summary, and obtains an `ExitCode`
    fn finish(&mut self) -> Result<ExitCode>;
}

struct MainAppImpl<'state, TermT, ProgressIndicatorT, ProgressDriverT, MainAppDepsT: MainAppDeps> {
    state: &'state MainAppState<MainAppDepsT>,
    queuing: JobQueuing<'state, ProgressIndicatorT, MainAppDepsT>,
    prog_driver: ProgressDriverT,
    prog: ProgressIndicatorT,
    term: TermT,
}

impl<'state, TermT, ProgressIndicatorT, ProgressDriverT, MainAppDepsT: MainAppDeps>
    MainAppImpl<'state, TermT, ProgressIndicatorT, ProgressDriverT, MainAppDepsT>
{
    fn new(
        state: &'state MainAppState<MainAppDepsT>,
        queuing: JobQueuing<'state, ProgressIndicatorT, MainAppDepsT>,
        prog_driver: ProgressDriverT,
        prog: ProgressIndicatorT,
        term: TermT,
    ) -> Self {
        Self {
            state,
            queuing,
            prog_driver,
            prog,
            term,
        }
    }
}

impl<'state, 'scope, TermT, ProgressIndicatorT, ProgressDriverT, MainAppDepsT> MainApp
    for MainAppImpl<'state, TermT, ProgressIndicatorT, ProgressDriverT, MainAppDepsT>
where
    ProgressIndicatorT: ProgressIndicator,
    TermT: TermLike + Clone + 'static,
    ProgressDriverT: ProgressDriver<'scope>,
    MainAppDepsT: MainAppDeps,
{
    fn enqueue_one(&mut self) -> Result<EnqueueResult> {
        self.queuing.enqueue_one()
    }

    fn drain(&mut self) -> Result<()> {
        slog::debug!(self.queuing.log, "draining");
        self.prog
            .update_length(self.state.queuing_state.jobs_queued.load(Ordering::Acquire));
        self.prog.done_queuing_jobs();
        self.prog_driver.stop()?;
        Ok(())
    }

    fn finish(&mut self) -> Result<ExitCode> {
        slog::debug!(self.queuing.log, "waiting for outstanding jobs");
        self.state.deps.wait_for_outstanding_jobs()?;
        self.prog.finished()?;

        if self.state.queuing_state.list_action.is_none() {
            let width = self.term.width() as usize;
            self.state
                .queuing_state
                .tracker
                .print_summary(width, self.term.clone())?;
        }

        write_test_listing(
            &self.state.cache_dir.join(LAST_TEST_LISTING_NAME),
            &self.state.queuing_state.test_listing.lock().unwrap(),
        )?;

        Ok(self.state.queuing_state.tracker.exit_code())
    }
}

fn list_packages<ProgressIndicatorT>(
    ind: &ProgressIndicatorT,
    packages: &BTreeMap<PackageId, CargoPackage>,
) where
    ProgressIndicatorT: ProgressIndicator,
{
    for pkg in packages.values() {
        ind.println(pkg.name.to_string());
    }
}

fn list_binaries<ProgressIndicatorT>(
    ind: &ProgressIndicatorT,
    packages: &BTreeMap<PackageId, CargoPackage>,
) where
    ProgressIndicatorT: ProgressIndicator,
{
    for pkg in packages.values() {
        for tgt in &pkg.targets {
            if tgt.test {
                let pkg_kind = pattern::ArtifactKind::from_target(tgt);
                let mut binary_name = String::new();
                if tgt.name != pkg.name {
                    binary_name += " ";
                    binary_name += &tgt.name;
                }
                ind.println(format!("{}{} ({})", &pkg.name, binary_name, pkg_kind));
            }
        }
    }
}

#[derive(Default)]
struct LoggingOutputInner {
    prog: Option<Box<dyn io::Write + Send + Sync + 'static>>,
}

#[derive(Clone, Default)]
pub struct LoggingOutput {
    inner: Arc<Mutex<LoggingOutputInner>>,
}

impl LoggingOutput {
    fn update(&self, prog: impl io::Write + Send + Sync + 'static) {
        let mut inner = self.inner.lock().unwrap();
        inner.prog = Some(Box::new(prog));
    }
}

impl io::Write for LoggingOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(prog) = &mut inner.prog {
            prog.write(buf)
        } else {
            io::stdout().write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(prog) = &mut inner.prog {
            prog.flush()
        } else {
            io::stdout().flush()
        }
    }
}

pub enum Logger {
    DefaultLogger(LogLevel),
    GivenLogger(slog::Logger),
}

impl Logger {
    pub fn build(&self, out: LoggingOutput) -> slog::Logger {
        match self {
            Self::DefaultLogger(level) => {
                let decorator = slog_term::PlainDecorator::new(out);
                let drain = slog_term::FullFormat::new(decorator).build().fuse();
                let drain = slog_async::Async::new(drain).build().fuse();
                let drain = slog::LevelFilter::new(drain, level.as_slog_level()).fuse();
                slog::Logger::root(drain, slog::o!())
            }
            Self::GivenLogger(logger) => logger.clone(),
        }
    }
}

fn new_helper<'state, 'scope, ProgressIndicatorT, TermT, MainAppDepsT>(
    state: &'state MainAppState<MainAppDepsT>,
    prog_factory: impl FnOnce(TermT) -> ProgressIndicatorT,
    term: TermT,
    mut prog_driver: impl ProgressDriver<'scope> + 'scope,
    timeout_override: Option<Option<Timeout>>,
) -> Result<Box<dyn MainApp + 'scope>>
where
    ProgressIndicatorT: ProgressIndicator,
    TermT: TermLike + Clone + 'static,
    MainAppDepsT: MainAppDeps,
    'state: 'scope,
{
    let width = term.width() as usize;
    let prog = prog_factory(term.clone());

    prog_driver.drive(&state.deps, prog.clone());
    prog.update_length(state.queuing_state.expected_job_count);

    state
        .logging_output
        .update(progress::ProgressWriteAdapter::new(prog.clone()));
    slog::debug!(state.log, "main app created");

    match state.queuing_state.list_action {
        Some(ListAction::ListPackages) => list_packages(&prog, &state.queuing_state.packages),

        Some(ListAction::ListBinaries) => list_binaries(&prog, &state.queuing_state.packages),
        _ => {}
    }

    let queuing = JobQueuing::new(
        state.log.clone(),
        &state.queuing_state,
        &state.deps,
        width,
        prog.clone(),
        timeout_override,
    )?;
    Ok(Box::new(MainAppImpl::new(
        state,
        queuing,
        prog_driver,
        prog,
        term,
    )))
}

/// Construct a `MainApp`
///
/// `state`: The shared state for the main app
/// `stdout_tty`: should terminal color codes be printed to stdout (provided via `term`)
/// `quiet`: indicates whether quiet mode should be used or not
/// `term`: represents the terminal
/// `driver`: drives the background work needed for updating the progress bars
pub fn main_app_new<'state, 'scope, TermT, MainAppDepsT>(
    state: &'state MainAppState<MainAppDepsT>,
    stdout_tty: bool,
    quiet: Quiet,
    term: TermT,
    driver: impl ProgressDriver<'scope> + 'scope,
    timeout_override: Option<Option<Timeout>>,
) -> Result<Box<dyn MainApp + 'scope>>
where
    TermT: TermLike + Clone + Send + Sync + UnwindSafe + RefUnwindSafe + 'static,
    MainAppDepsT: MainAppDeps,
    'state: 'scope,
{
    if state.queuing_state.list_action.is_some() {
        return if stdout_tty {
            Ok(new_helper(
                state,
                TestListingProgress::new,
                term,
                driver,
                timeout_override,
            )?)
        } else {
            Ok(new_helper(
                state,
                TestListingProgressNoSpinner::new,
                term,
                driver,
                timeout_override,
            )?)
        };
    }

    match (stdout_tty, quiet.into_inner()) {
        (true, true) => Ok(new_helper(
            state,
            QuietProgressBar::new,
            term,
            driver,
            timeout_override,
        )?),
        (true, false) => Ok(new_helper(
            state,
            MultipleProgressBars::new,
            term,
            driver,
            timeout_override,
        )?),
        (false, true) => Ok(new_helper(
            state,
            QuietNoBar::new,
            term,
            driver,
            timeout_override,
        )?),
        (false, false) => Ok(new_helper(
            state,
            NoBar::new,
            term,
            driver,
            timeout_override,
        )?),
    }
}
