//! Easily start and stop processes.

use crate::config::InlineLimit;
use anyhow::{anyhow, Error, Result};
use futures::ready;
use meticulous_base::{JobDetails, JobOutputResult};
use nix::{
    errno::Errno,
    fcntl::{self, FcntlArg, OFlag},
    unistd::{self, Gid, Pid, Uid},
};
use std::{
    ffi::{c_char, CString},
    fs::File,
    io::Read as _,
    iter, mem,
    os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, OwnedFd},
    pin::Pin,
    ptr,
    task::{Context, Poll},
};
use tokio::{
    io::{self, unix::AsyncFd, AsyncRead, AsyncReadExt as _, ReadBuf},
    task,
};
use tuple::Map as _;

/*              _     _ _
 *  _ __  _   _| |__ | (_) ___
 * | '_ \| | | | '_ \| | |/ __|
 * | |_) | |_| | |_) | | | (__
 * | .__/ \__,_|_.__/|_|_|\___|
 * |_|
 *  FIGLET: public
 */

pub struct Executor {
    uid: Uid,
    gid: Gid,
}

impl Default for Executor {
    fn default() -> Self {
        let uid = unistd::getuid();
        let gid = unistd::getgid();
        Executor { uid, gid }
    }
}

/// Return value from [`start`]. See [`meticulous_base::JobResult`] for an explanation of the
/// [`StartResult::ExecutionError`] and [`StartResult::SystemError`] variants.
#[derive(Debug)]
pub enum StartResult {
    Ok(Pid),
    ExecutionError(Error),
    SystemError(Error),
}

impl Executor {
    /// Start a process (i.e. job).
    ///
    /// Two callbacks are provided: one for stdout and one for stderr. These will be called on a
    /// separate task (they should not block) when the job has closed its stdout/stderr. This will
    /// likely happen when the job completes.
    ///
    /// No callback is called when the process actually terminates. For that, the caller should use
    /// waitid(2) or something similar to wait on the pid returned from this function. In
    /// production, that role will be filled by [`crate::reaper::main`].
    ///
    /// This function is designed to be callable in an async context, even though it temporarily
    /// blocks the calling thread while the child is starting up.
    ///
    /// If this function returns [`StartResult::Ok`], then the child process obviously will be
    /// started and the caller will need to waitid(2) on the child eventually. However, if this
    /// function returns an error result, it's still possible that a child was spawned (and has now
    /// terminated). It is assumed that the caller will be reaping all children, not just those
    /// positively identified by this function. If that assumption proves invalid, the return
    /// values of this function should be adjusted to return optional pids in error cases.
    #[must_use]
    pub fn start(
        &self,
        details: &JobDetails,
        inline_limit: InlineLimit,
        stdout_done: impl FnOnce(Result<JobOutputResult>) + Send + 'static,
        stderr_done: impl FnOnce(Result<JobOutputResult>) + Send + 'static,
    ) -> StartResult {
        self.start_inner(details, inline_limit, stdout_done, stderr_done)
    }
}

/*             _            _
 *  _ __  _ __(_)_   ____ _| |_ ___
 * | '_ \| '__| \ \ / / _` | __/ _ \
 * | |_) | |  | |\ V / (_| | ||  __/
 * | .__/|_|  |_| \_/ \__,_|\__\___|
 * |_|
 *  FIGLET: private
 */

/// A wrapper for a raw, non-blocking fd that allows it to be read from async code.
struct AsyncFile(AsyncFd<File>);

impl AsyncRead for AsyncFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;

            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| inner.get_ref().read(unfilled)) {
                Ok(Ok(len)) => {
                    buf.advance(len);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Read all of the contents of `stream` and return the appropriate [`JobOutputResult`].
async fn output_reader(
    inline_limit: InlineLimit,
    stream: impl AsyncRead + std::marker::Unpin,
) -> Result<JobOutputResult> {
    let mut buf = Vec::<u8>::new();
    let mut take = stream.take(inline_limit.into_inner());
    take.read_to_end(&mut buf).await?;
    let buf = buf.into_boxed_slice();
    let truncated = io::copy(&mut take.into_inner(), &mut io::sink()).await?;
    match truncated {
        0 if buf.is_empty() => Ok(JobOutputResult::None),
        0 => Ok(JobOutputResult::Inline(buf)),
        _ => Ok(JobOutputResult::Truncated {
            first: buf,
            truncated,
        }),
    }
}

/// Task main for the output reader: Read the output and then call the callback.
async fn output_reader_task_main(
    inline_limit: InlineLimit,
    stream: impl AsyncRead + std::marker::Unpin,
    done: impl FnOnce(Result<JobOutputResult>) + Send + 'static,
) {
    done(output_reader(inline_limit, stream).await);
}

impl Executor {
    #[must_use]
    fn start_inner(
        &self,
        details: &JobDetails,
        inline_limit: InlineLimit,
        stdout_done: impl FnOnce(Result<JobOutputResult>) + Send + 'static,
        stderr_done: impl FnOnce(Result<JobOutputResult>) + Send + 'static,
    ) -> StartResult {
        macro_rules! try_system_error {
            ($e:expr) => {
                match $e {
                    Ok(val) => val,
                    Err(err) => return StartResult::SystemError(err.into()),
                }
            };
        }

        // We're going to need three pipes: one for stdout, one for stderr, and one to convey back any
        // error that occurs in the child before it execs. It's easiest to create the pipes in the
        // parent before cloning and then closing the unnecessary ends in the parent and child.
        let (stdout_read_fd, stdout_write_fd) =
            try_system_error!(unistd::pipe()).map(|raw_fd| unsafe { OwnedFd::from_raw_fd(raw_fd) });
        let (stderr_read_fd, stderr_write_fd) =
            try_system_error!(unistd::pipe()).map(|raw_fd| unsafe { OwnedFd::from_raw_fd(raw_fd) });
        let (exec_result_read_fd, exec_result_write_fd) =
            try_system_error!(unistd::pipe()).map(|raw_fd| unsafe { OwnedFd::from_raw_fd(raw_fd) });

        // Set up the arguments to pass to the child process.
        let program = try_system_error!(CString::new(details.program.as_str()));
        let program_ptr: *const u8 = program.as_bytes_with_nul().as_ptr();
        let arguments = try_system_error!(details
            .arguments
            .iter()
            .map(String::as_str)
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>());
        let argv = iter::once(program_ptr)
            .chain(
                arguments
                    .iter()
                    .map(|cstr| cstr.as_bytes_with_nul().as_ptr()),
            )
            .chain(iter::once(ptr::null()))
            .collect::<Vec<_>>();
        let argv_ptr: *const *const u8 = argv.as_ptr();
        let env: [*const u8; 1] = [ptr::null()];
        let env_ptr: *const *const u8 = env.as_ptr();

        // Do the clone.
        let mut clone_args = nc::clone_args_t {
            flags: nc::CLONE_NEWCGROUP as u64
                | nc::CLONE_NEWIPC as u64
                // If we create a new network namespace, we probably need to configure it
                // | nc::CLONE_NEWNET as u64
                | nc::CLONE_NEWNS as u64
                | nc::CLONE_NEWPID as u64
                | nc::CLONE_NEWUSER as u64,
            exit_signal: nc::SIGCHLD as u64,
            ..Default::default()
        };
        let child_pid =
            match unsafe { nc::clone3(&mut clone_args, mem::size_of::<nc::clone_args_t>()) } {
                Ok(val) => val,
                Err(err) => {
                    return StartResult::SystemError(Errno::from_i32(err).into());
                }
            };
        if child_pid == 0 {
            // This is the child process.
            
            // WARNING: We have to be extremely careful to not call any library functions that may
            // block on another thread, like allocating memory. When a multi-threaded process is
            // cloned, only the calling process is cloned into the child. From the child's point of
            // view, it's like those other threads just disappeared. If those threads held any
            // locks at the point when the process was cloned, thos locks effectively become dead
            // in the child. This means that the child has to assume that every lock is dead, and
            // must not try to acquire them. This is why the function we're calling lives in a
            // separate, no_std, crate.

            // N.B. We don't close any file descriptors here, like stdout_read_fd, stderr_read_fd,
            // and exec_result_read_fd, because they will automatically be closed when the child
            // execs.

            unsafe {
                meticulous_worker_child::start_and_exec_in_child(
                    program_ptr as *const c_char,
                    argv_ptr as *const *const c_char,
                    env_ptr as *const *const c_char,
                    stdout_write_fd.into_raw_fd(),
                    stderr_write_fd.into_raw_fd(),
                    exec_result_write_fd.into_raw_fd(),
                    self.uid.as_raw(),
                    self.gid.as_raw(),
                )
            };
        }

        // At this point, it's still okay to return early in the parent. The child will continue to
        // execute, but that's okay. If it writes to one of the pipes, it will receive a SIGPIPE.
        // Otherwise, it will continue until it's done, and then we'll reap the zombie. Since we won't
        // have any jid->pid association, we'll just ignore the result.

        // Drop the write sides of the pipes in the parent. It's important that we drop
        // exec_result_write_fd before reading from that pipe next.
        drop(stdout_write_fd);
        drop(stderr_write_fd);
        drop(exec_result_write_fd);

        // Read (in a blocking manner) from the exec result pipe. The child will write to the pipe if
        // it has an error exec-ing. The child will mark the write side of the pipe exec-on-close, so
        // we'll read an immediate EOF if the exec is successful.
        let mut exec_result_buf = vec![];
        try_system_error!(File::from(exec_result_read_fd).read_to_end(&mut exec_result_buf));
        if !exec_result_buf.is_empty() {
            return StartResult::ExecutionError(anyhow!(
                "exec-ing job's process: {}",
                match meticulous_worker_child::Error::try_from(exec_result_buf.as_slice()) {
                    Err(err) => {
                        err
                    }
                    Ok(meticulous_worker_child::Error::Errno(errno)) => {
                        Errno::from_i32(errno).desc()
                    }
                    Ok(meticulous_worker_child::Error::BufferTooSmall) => {
                        "buffer_to_small"
                    }
                }
            ));
        }

        // Make the read side of the stdout and stderr pipes non-blocking so that we can use them with
        // Tokio.
        try_system_error!(fcntl::fcntl(
            stdout_read_fd.as_raw_fd(),
            FcntlArg::F_SETFL(OFlag::O_NONBLOCK)
        ));
        try_system_error!(fcntl::fcntl(
            stderr_read_fd.as_raw_fd(),
            FcntlArg::F_SETFL(OFlag::O_NONBLOCK)
        ));

        // Spawn reader tasks to consume stdout and stderr.
        task::spawn(output_reader_task_main(
            inline_limit,
            AsyncFile(try_system_error!(AsyncFd::new(File::from(stdout_read_fd)))),
            stdout_done,
        ));
        task::spawn(output_reader_task_main(
            inline_limit,
            AsyncFile(try_system_error!(AsyncFd::new(File::from(stderr_read_fd)))),
            stderr_done,
        ));

        StartResult::Ok(Pid::from_raw(child_pid))
    }
}

/*  _            _
 * | |_ ___  ___| |_ ___
 * | __/ _ \/ __| __/ __|
 * | ||  __/\__ \ |_\__ \
 *  \__\___||___/\__|___/
 *  FIGLET: tests
 */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reaper::{self, ReaperDeps, ReaperInstruction};
    use assert_matches::*;
    use meticulous_base::JobStatus;
    use meticulous_test::boxed_u8;
    use nix::sys::signal::{self, Signal};
    use serial_test::serial;
    use tokio::sync::oneshot;

    macro_rules! bash {
        ($($tokens:expr),*) => {
            JobDetails {
                program: "/usr/bin/bash".to_string(),
                arguments: vec![
                    "-c".to_string(),
                    format!($($tokens),*),
                ],
                layers: vec![],
            }
        };
    }

    macro_rules! python {
        ($($tokens:expr),*) => {
            JobDetails {
                program: "/usr/bin/python3".to_string(),
                arguments: vec![
                    "-c".to_string(),
                    format!($($tokens),*),
                ],
                layers: vec![],
            }
        };
    }

    struct ReaperAdapter {
        pid: Pid,
        result: Option<JobStatus>,
    }

    impl ReaperAdapter {
        fn new(pid: Pid) -> Self {
            ReaperAdapter { pid, result: None }
        }
    }

    impl ReaperDeps for &mut ReaperAdapter {
        fn on_waitid_error(&mut self, err: Errno) -> ReaperInstruction {
            panic!("waitid error: {err}");
        }
        fn on_dummy_child_termination(&mut self) -> ReaperInstruction {
            panic!("dummy child panicked");
        }
        fn on_unexpected_wait_code(&mut self, _pid: Pid) -> ReaperInstruction {
            panic!("unexpected wait code");
        }
        fn on_child_termination(&mut self, pid: Pid, status: JobStatus) -> ReaperInstruction {
            if self.pid == pid {
                self.result = Some(status);
                ReaperInstruction::Stop
            } else {
                ReaperInstruction::Continue
            }
        }
    }

    async fn start_and_expect(
        details: JobDetails,
        inline_limit: u64,
        expected_status: JobStatus,
        expected_stdout: JobOutputResult,
        expected_stderr: JobOutputResult,
    ) {
        let dummy_child_pid = reaper::clone_dummy_child().unwrap();
        let (stdout_tx, stdout_rx) = oneshot::channel();
        let (stderr_tx, stderr_rx) = oneshot::channel();
        let start_result = Executor::default().start(
            &details,
            InlineLimit::from(inline_limit),
            |stdout| stdout_tx.send(stdout.unwrap()).unwrap(),
            |stderr| stderr_tx.send(stderr.unwrap()).unwrap(),
        );
        assert_matches!(start_result, StartResult::Ok(_));
        let StartResult::Ok(pid) = start_result else {
            unreachable!();
        };
        let reaper = task::spawn_blocking(move || {
            let mut adapter = ReaperAdapter::new(pid);
            reaper::main(&mut adapter, dummy_child_pid);
            let result = adapter.result.unwrap();
            signal::kill(dummy_child_pid, Signal::SIGKILL).ok();
            let mut adapter = ReaperAdapter::new(dummy_child_pid);
            reaper::main(&mut adapter, Pid::from_raw(0));
            result
        });
        assert_eq!(reaper.await.unwrap(), expected_status);
        assert_eq!(stdout_rx.await.unwrap(), expected_stdout);
        assert_eq!(stderr_rx.await.unwrap(), expected_stderr);
    }

    #[tokio::test]
    #[serial]
    async fn exited_0() {
        start_and_expect(
            bash!("exit 0"),
            0,
            JobStatus::Exited(0),
            JobOutputResult::None,
            JobOutputResult::None,
        )
        .await;
    }

    #[tokio::test]
    #[serial]
    async fn exited_1() {
        start_and_expect(
            bash!("exit 1"),
            0,
            JobStatus::Exited(1),
            JobOutputResult::None,
            JobOutputResult::None,
        )
        .await;
    }

    // $$ returns the pid of outer-most bash. This doesn't do what we expect it to do when using
    // our executor. We should probably rewrite these tests to run python or something, and take
    // input from stdin.
    #[tokio::test]
    #[serial]
    async fn signaled_11() {
        start_and_expect(
            python!(concat!(
                "import os;",
                "import sys;",
                "print('a');",
                "sys.stdout.flush();",
                "print('b', file=sys.stderr);",
                "os.abort()",
            )),
            2,
            JobStatus::Signaled(11),
            JobOutputResult::Inline(boxed_u8!(b"a\n")),
            JobOutputResult::Inline(boxed_u8!(b"b\n")),
        )
        .await;
    }

    #[tokio::test]
    #[serial]
    async fn stdout() {
        start_and_expect(
            bash!("echo a"),
            0,
            JobStatus::Exited(0),
            JobOutputResult::Truncated {
                first: boxed_u8!(b""),
                truncated: 2,
            },
            JobOutputResult::None,
        )
        .await;
        start_and_expect(
            bash!("echo a"),
            1,
            JobStatus::Exited(0),
            JobOutputResult::Truncated {
                first: boxed_u8!(b"a"),
                truncated: 1,
            },
            JobOutputResult::None,
        )
        .await;
        start_and_expect(
            bash!("echo a"),
            2,
            JobStatus::Exited(0),
            JobOutputResult::Inline(boxed_u8!(b"a\n")),
            JobOutputResult::None,
        )
        .await;
        start_and_expect(
            bash!("echo a"),
            3,
            JobStatus::Exited(0),
            JobOutputResult::Inline(boxed_u8!(b"a\n")),
            JobOutputResult::None,
        )
        .await;
    }

    #[tokio::test]
    #[serial]
    async fn stderr() {
        start_and_expect(
            bash!("echo a >&2"),
            0,
            JobStatus::Exited(0),
            JobOutputResult::None,
            JobOutputResult::Truncated {
                first: boxed_u8!(b""),
                truncated: 2,
            },
        )
        .await;
        start_and_expect(
            bash!("echo a >&2"),
            1,
            JobStatus::Exited(0),
            JobOutputResult::None,
            JobOutputResult::Truncated {
                first: boxed_u8!(b"a"),
                truncated: 1,
            },
        )
        .await;
        start_and_expect(
            bash!("echo a >&2"),
            2,
            JobStatus::Exited(0),
            JobOutputResult::None,
            JobOutputResult::Inline(boxed_u8!(b"a\n")),
        )
        .await;
        start_and_expect(
            bash!("echo a >&2"),
            3,
            JobStatus::Exited(0),
            JobOutputResult::None,
            JobOutputResult::Inline(boxed_u8!(b"a\n")),
        )
        .await;
    }

    #[test]
    #[serial]
    fn execution_error() {
        let details = JobDetails {
            program: "a_program_that_does_not_exist".to_string(),
            arguments: vec![],
            layers: vec![],
        };
        assert_matches!(
            Executor::default().start(&details, 0.into(), |_| unreachable!(), |_| unreachable!()),
            StartResult::ExecutionError(_)
        );
    }
}
