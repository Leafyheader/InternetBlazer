//! Child processes that are stopped together with everything they started, such as yt-dlp and
//! the ffmpeg it runs. Killing only yt-dlp would leave those behind, still writing the files.

use std::{
    io,
    process::{Output, Stdio},
};

use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

/// A running process and the processes it starts. Dropping it kills them all.
pub(crate) struct Tree {
    child: Child,
    /// Every process in the job ends when its last handle closes, even if the app crashes.
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

impl Tree {
    /// Starts `cmd` without a console window of its own.
    pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Self> {
        cmd.kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn()?;
        #[cfg(windows)]
        {
            // The yt-dlp.exe bootloader unpacks itself before it starts its real process, so it
            // joins the job long before it has children; those join it too.
            let job = child.raw_handle().and_then(|process| {
                windows::job_for(process)
                    .inspect_err(|e| tracing::warn!("could not group the child processes: {e}"))
                    .ok()
            });
            Ok(Self { child, job })
        }
        #[cfg(not(windows))]
        Ok(Self { child })
    }

    /// Runs `cmd` to the end and collects what it prints. Dropping the future stops it.
    pub(crate) async fn output(cmd: &mut Command) -> io::Result<Output> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut tree = Self::spawn(cmd)?;
        let (Some(mut stdout), Some(mut stderr)) =
            (tree.child.stdout.take(), tree.child.stderr.take())
        else {
            unreachable!("both are piped");
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (read_out, read_err) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        read_out?;
        read_err?;
        let status = tree.child.wait().await?;
        Ok(Output { status, stdout: out, stderr: err })
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Ends the processes and waits for the first one to exit.
    pub(crate) async fn kill(&mut self) {
        #[cfg(windows)]
        drop(self.job.take());
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The group shares the first process's ID, as `process_group(0)` made it.
            // SAFETY: plain system call.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
        let _ = self.child.kill().await;
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        io, mem,
        os::windows::io::{FromRawHandle, OwnedHandle, RawHandle},
        ptr,
    };

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// A job holding `process` that kills everything in it once its handle closes.
    pub(super) fn job_for(process: RawHandle) -> io::Result<OwnedHandle> {
        // SAFETY: the job handle is checked before use and owned by the returned value;
        // `process` is a live handle owned by the caller's `Child`.
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = OwnedHandle::from_raw_handle(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let handle = std::os::windows::io::AsRawHandle::as_raw_handle(&job);
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, process) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }
}
