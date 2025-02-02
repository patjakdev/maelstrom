use crate::ProgressIndicator;
use anyhow::Result;
use colored::{ColoredString, Colorize as _};
use indicatif::TermLike;
use maelstrom_base::{
    ClientJobId, JobCompleted, JobEffects, JobError, JobOutcome, JobOutcomeResult, JobOutputResult,
    JobStatus,
};
use maelstrom_util::process::{ExitCode, ExitCodeAccumulator};
use std::sync::{Arc, Mutex};
use unicode_truncate::UnicodeTruncateStr as _;
use unicode_width::UnicodeWidthStr as _;

enum CaseResult {
    Ignored,
    Ran(ExitCode),
}

#[derive(Default)]
pub struct JobStatusTracker {
    statuses: Mutex<Vec<(String, CaseResult)>>,
    exit_code: ExitCodeAccumulator,
}

impl JobStatusTracker {
    pub fn job_exited(&self, case: String, exit_code: ExitCode) {
        let mut statuses = self.statuses.lock().unwrap();
        statuses.push((case, CaseResult::Ran(exit_code)));
        self.exit_code.add(exit_code);
    }

    pub fn job_ignored(&self, case: String) {
        let mut statuses = self.statuses.lock().unwrap();
        statuses.push((case, CaseResult::Ignored));
    }

    pub fn print_summary(&self, width: usize, term: impl TermLike) -> Result<()> {
        term.write_line("")?;

        let heading = " Test Summary ";
        let equal_width = (width - heading.width()) / 2;
        term.write_line(&format!(
            "{empty:=<equal_width$}{heading}{empty:=<equal_width$}",
            empty = ""
        ))?;

        let success = "Successful Tests";
        let failure = "Failed Tests";
        let ignore = "Ignored Tests";
        let mut column1_width = std::cmp::max(success.width(), failure.width());
        let max_digits = 9;
        let statuses = self.statuses.lock().unwrap();
        let failed = statuses
            .iter()
            .filter(|(_, res)| matches!(res, CaseResult::Ran(e) if e != &ExitCode::SUCCESS));
        let ignored = statuses
            .iter()
            .filter(|(_, res)| matches!(res, CaseResult::Ignored));
        let num_failed = failed.clone().count();
        let num_ignored = ignored.clone().count();
        let num_succeeded = statuses.len() - num_failed - num_ignored;

        if num_ignored > 0 {
            column1_width = std::cmp::max(column1_width, ignore.width());
        }

        term.write_line(&format!(
            "{:<column1_width$}: {num_succeeded:>max_digits$}",
            success.green(),
        ))?;
        term.write_line(&format!(
            "{:<column1_width$}: {num_failed:>max_digits$}",
            failure.red(),
        ))?;
        let failed_width = failed.clone().map(|(n, _)| n.width()).max().unwrap_or(0);
        for (failed, _) in failed {
            term.write_line(&format!("    {failed:<failed_width$}: {}", "failure".red()))?;
        }

        if num_ignored > 0 {
            term.write_line(&format!(
                "{:<column1_width$}: {num_ignored:>max_digits$}",
                ignore.yellow(),
            ))?;
            let failed_width = ignored.clone().map(|(n, _)| n.width()).max().unwrap_or(0);
            for (ignored, _) in ignored {
                term.write_line(&format!(
                    "    {ignored:<failed_width$}: {}",
                    "ignored".yellow()
                ))?;
            }
        }

        term.flush()?;
        Ok(())
    }

    pub fn exit_code(&self) -> ExitCode {
        self.exit_code.get()
    }
}

pub struct JobStatusVisitor<ProgressIndicatorT> {
    tracker: Arc<JobStatusTracker>,
    case: String,
    width: usize,
    ind: ProgressIndicatorT,
}

impl<ProgressIndicatorT> JobStatusVisitor<ProgressIndicatorT> {
    pub fn new(
        tracker: Arc<JobStatusTracker>,
        case: String,
        width: usize,
        ind: ProgressIndicatorT,
    ) -> Self {
        Self {
            tracker,
            case,
            width,
            ind,
        }
    }
}

fn format_test_output(res: &JobOutputResult, name: &str, cjid: ClientJobId) -> Vec<String> {
    let mut test_output_lines = vec![];
    match res {
        JobOutputResult::None => {}
        JobOutputResult::Inline(bytes) => {
            test_output_lines.push(String::from_utf8_lossy(bytes).into());
        }
        JobOutputResult::Truncated { first, truncated } => {
            test_output_lines.push(String::from_utf8_lossy(first).into());
            test_output_lines.push(format!(
                "job {cjid}: {name} truncated, {truncated} bytes lost"
            ));
        }
    }
    test_output_lines
}

impl<ProgressIndicatorT: ProgressIndicator> JobStatusVisitor<ProgressIndicatorT> {
    fn print_job_result(&self, result_str: ColoredString, duration_str: String) {
        if self.width > 10 {
            let case_width = self.case.width();
            let trailer_str = format!("{result_str} {duration_str:>8}");
            let trailer_width = result_str.width() + 1 + std::cmp::max(duration_str.width(), 8);
            if case_width + trailer_width < self.width {
                let dots_width = self.width - trailer_width - case_width;
                let case = self.case.bold();

                self.ind.println(format!(
                    "{case}{empty:.<dots_width$}{trailer_str}",
                    empty = "",
                ));
            } else {
                let (case, case_width) = self
                    .case
                    .unicode_truncate_start(self.width - 2 - trailer_width);
                let case = case.bold();
                let dots_width = self.width - trailer_width - case_width - 1;
                self.ind.println(format!(
                    "<{case}{empty:.<dots_width$}{trailer_str}",
                    empty = ""
                ));
            }
        } else {
            self.ind
                .println(format!("{case} {result_str}", case = self.case));
        }
    }

    pub fn job_finished(&self, cjid: ClientJobId, result: JobOutcomeResult) {
        let result_str: ColoredString;
        let mut result_details: Option<String> = None;
        let mut test_output_stderr: Vec<String> = vec![];
        let mut test_output_stdout: Vec<String> = vec![];
        let mut duration_str = String::new();
        match result {
            Ok(JobOutcome::Completed(JobCompleted {
                status,
                effects:
                    JobEffects {
                        stdout,
                        stderr,
                        duration,
                        ..
                    },
            })) => {
                duration_str = format!("{:.3}s", duration.as_secs_f64());
                let mut job_failed = true;
                match status {
                    JobStatus::Exited(code) => {
                        result_str = if code == 0 {
                            job_failed = false;
                            "OK".green()
                        } else {
                            "FAIL".red()
                        };
                        self.tracker
                            .job_exited(self.case.clone(), ExitCode::from(code));
                    }
                    JobStatus::Signaled(signo) => {
                        result_str = "FAIL".red();
                        result_details = Some(format!("killed by signal {signo}"));
                        self.tracker
                            .job_exited(self.case.clone(), ExitCode::FAILURE);
                    }
                };
                if job_failed {
                    test_output_stdout.extend(format_test_output(&stdout, "stdout", cjid));
                    test_output_stderr.extend(format_test_output(&stderr, "stderr", cjid));
                }
            }
            Ok(JobOutcome::TimedOut(JobEffects { stdout, stderr, .. })) => {
                result_str = "TIMEOUT".red();
                result_details = Some("timed out".into());
                self.tracker
                    .job_exited(self.case.clone(), ExitCode::FAILURE);
                test_output_stdout.extend(format_test_output(&stdout, "stdout", cjid));
                test_output_stderr.extend(format_test_output(&stderr, "stderr", cjid));
            }
            Err(JobError::Execution(err)) => {
                result_str = "ERR".yellow();
                result_details = Some(format!("execution error: {err}"));
                self.tracker
                    .job_exited(self.case.clone(), ExitCode::FAILURE);
            }
            Err(JobError::System(err)) => {
                result_str = "ERR".yellow();
                result_details = Some(format!("system error: {err}"));
                self.tracker
                    .job_exited(self.case.clone(), ExitCode::FAILURE);
            }
        }
        self.print_job_result(result_str, duration_str);

        if let Some(details_str) = result_details {
            self.ind.println(details_str);
        }
        for line in test_output_stdout {
            self.ind.println(line);
        }
        for line in test_output_stderr {
            self.ind.eprintln(line);
        }
        self.ind.job_finished();
    }

    pub fn job_ignored(&self) {
        self.print_job_result("IGNORED".yellow(), "".into());
        self.tracker.job_ignored(self.case.clone());
        self.ind.job_finished();
    }
}
