use anyhow::{anyhow, Result};
use cargo::{get_cases_from_binary, CargoBuild};
use cargo_metadata::Artifact as CargoArtifact;
use clap::Parser;
use console::Term;
use indicatif::TermLike;
use meticulous_base::JobDetails;
use meticulous_client::Client;
use meticulous_util::process::ExitCode;
use progress::{
    MultipleProgressBars, NoBar, ProgressIndicator, ProgressIndicatorScope, QuietNoBar,
    QuietProgressBar,
};
use std::collections::HashSet;
use std::io::IsTerminal as _;
use std::{
    io::{self},
    net::{SocketAddr, ToSocketAddrs as _},
    str,
    sync::{Arc, Mutex},
};
use visitor::{JobStatusTracker, JobStatusVisitor};

mod cargo;
mod progress;
mod visitor;

fn parse_socket_addr(arg: &str) -> io::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = arg.to_socket_addrs()?.collect();
    // It's not clear how we could end up with an empty iterator. We'll assume
    // that's impossible until proven wrong.
    Ok(*addrs.get(0).unwrap())
}

/// The meticulous client. This process sends work to the broker to be executed by workers.
#[derive(Parser)]
#[command(version, bin_name = "cargo")]
struct Cli {
    #[clap(subcommand)]
    subcommand: Subcommand,
}

impl Cli {
    fn subcommand(self) -> MetestCli {
        match self.subcommand {
            Subcommand::Metest(cmd) => cmd,
        }
    }
}

#[derive(Debug, clap::Args)]
struct MetestCli {
    /// Socket address of broker. Examples: 127.0.0.1:5000 host.example.com:2000".
    #[arg(value_parser = parse_socket_addr)]
    broker: SocketAddr,
    /// Don't output information about the tests being run
    #[arg(short, long)]
    quiet: bool,
    /// Only run tests from the given package
    #[arg(short, long)]
    package: Option<String>,
    /// Only run tests whose names contain the given string
    filter: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum Subcommand {
    Metest(MetestCli),
}

struct JobQueuer<StdErr> {
    cargo: String,
    package: Option<String>,
    filter: Option<String>,
    stderr: StdErr,
    stderr_color: bool,
    tracker: Arc<JobStatusTracker>,
    jobs_queued: u64,
}

impl<StdErr> JobQueuer<StdErr> {
    fn new(
        cargo: String,
        package: Option<String>,
        filter: Option<String>,
        stderr: StdErr,
        stderr_color: bool,
    ) -> Self {
        Self {
            cargo,
            package,
            filter,
            stderr,
            stderr_color,
            tracker: Arc::new(JobStatusTracker::default()),
            jobs_queued: 0,
        }
    }
}

impl<StdErr: io::Write> JobQueuer<StdErr> {
    fn queue_job_from_case<ProgressIndicatorT>(
        &mut self,
        client: &Mutex<Client>,
        width: usize,
        ind: ProgressIndicatorT,
        ignored_cases: &HashSet<String>,
        package_name: &str,
        case: &str,
        binary: &str,
    ) -> Result<()>
    where
        ProgressIndicatorT: ProgressIndicatorScope,
    {
        let case_str = format!("{package_name} {case}");
        let visitor = JobStatusVisitor::new(self.tracker.clone(), case_str, width, ind);

        if ignored_cases.contains(case) {
            visitor.job_ignored();
            return Ok(());
        }

        client.lock().unwrap().add_job(
            JobDetails {
                program: binary.into(),
                arguments: vec!["--exact".into(), "--nocapture".into(), case.into()],
                environment: vec![],
                layers: vec![],
            },
            Box::new(move |cjid, result| visitor.job_finished(cjid, result)),
        );

        Ok(())
    }

    fn queue_jobs_from_artifact<ProgressIndicatorT>(
        &mut self,
        client: &Mutex<Client>,
        width: usize,
        ind: ProgressIndicatorT,
        cb: &mut impl FnMut(u64),
        artifact: CargoArtifact,
    ) -> Result<bool>
    where
        ProgressIndicatorT: ProgressIndicatorScope,
    {
        let package_name = artifact.package_id.repr.split(' ').next().unwrap();
        if let Some(package) = &self.package {
            if package_name != package {
                return Ok(false);
            }
        }

        let binary = artifact.executable.unwrap().to_string();
        let ignored_cases: HashSet<_> = get_cases_from_binary(&binary, &Some("--ignored".into()))?
            .into_iter()
            .collect();

        for case in get_cases_from_binary(&binary, &self.filter)? {
            self.jobs_queued += 1;
            cb(self.jobs_queued);

            self.queue_job_from_case(
                client,
                width,
                ind.clone(),
                &ignored_cases,
                package_name,
                &case,
                &binary,
            )?;
        }

        Ok(true)
    }

    fn queue_jobs_and_wait<ProgressIndicatorT>(
        mut self,
        client: &Mutex<Client>,
        width: usize,
        ind: ProgressIndicatorT,
        mut cb: impl FnMut(u64),
    ) -> Result<()>
    where
        ProgressIndicatorT: ProgressIndicatorScope,
    {
        let mut cargo_build =
            CargoBuild::new(&self.cargo, self.stderr_color, self.package.clone())?;

        let mut package_match = false;

        for artifact in cargo_build.artifact_stream() {
            let artifact = artifact?;
            package_match |=
                self.queue_jobs_from_artifact(client, width, ind.clone(), &mut cb, artifact)?;
        }

        cargo_build.check_status(self.stderr)?;

        if let Some(package) = self.package {
            if !package_match {
                return Err(anyhow!("package {package:?} unknown"));
            }
        }

        Ok(())
    }
}

pub struct MainApp<StdErr> {
    client: Mutex<Client>,
    queuer: JobQueuer<StdErr>,
}

impl<StdErr> MainApp<StdErr> {
    fn new(
        client: Mutex<Client>,
        cargo: String,
        package: Option<String>,
        filter: Option<String>,
        stderr: StdErr,
        stderr_color: bool,
    ) -> Self {
        Self {
            client,
            queuer: JobQueuer::new(cargo, package, filter, stderr, stderr_color),
        }
    }
}

impl<StdErr: io::Write> MainApp<StdErr> {
    fn run_with_progress<ProgressIndicatorT, Term>(
        self,
        prog_factory: impl FnOnce(Term) -> ProgressIndicatorT,
        term: Term,
    ) -> Result<ExitCode>
    where
        ProgressIndicatorT: ProgressIndicator,
        Term: TermLike + Clone + 'static,
    {
        let width = term.width() as usize;
        let prog = prog_factory(term.clone());
        let tracker = self.queuer.tracker.clone();

        prog.run(self.client, |client, bar_scope| {
            let cb = |num_jobs| bar_scope.update_length(num_jobs);
            self.queuer
                .queue_jobs_and_wait(client, width, bar_scope.clone(), cb)
        })?;

        tracker.print_summary(width, term)?;
        Ok(tracker.exit_code())
    }

    fn run<Term>(self, stdout_tty: bool, quiet: bool, term: Term) -> Result<ExitCode>
    where
        Term: TermLike + Clone + Send + Sync + 'static,
    {
        match (stdout_tty, quiet) {
            (true, true) => Ok(self.run_with_progress(QuietProgressBar::new, term)?),
            (true, false) => Ok(self.run_with_progress(MultipleProgressBars::new, term)?),
            (false, true) => Ok(self.run_with_progress(QuietNoBar::new, term)?),
            (false, false) => Ok(self.run_with_progress(NoBar::new, term)?),
        }
    }
}

/// The main function for the client. This should be called on a task of its own. It will return
/// when a signal is received or when all work has been processed by the broker.
pub fn main() -> Result<ExitCode> {
    let cli_options = Cli::parse().subcommand();
    let client = Mutex::new(Client::new(cli_options.broker)?);
    let app = MainApp::new(
        client,
        "cargo".into(),
        cli_options.package,
        cli_options.filter,
        std::io::stderr().lock(),
        std::io::stderr().is_terminal(),
    );

    let stdout_tty = std::io::stdout().is_terminal();
    app.run(stdout_tty, cli_options.quiet, Term::buffered_stdout())
}

#[test]
fn test_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}

#[cfg(test)]
mod integration_test;
