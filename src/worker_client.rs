use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};

pub struct Worker {
    child: Child,
    input: ChildStdin,
    pub messages: Receiver<serde_json::Value>,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl Worker {
    pub fn start(
        exe_dir: &Path,
        database: &Path,
        roots: &[PathBuf],
        exclusions: &[PathBuf],
        dynamic_fixed: bool,
        wake: eframe::egui::Context,
    ) -> Result<Self> {
        let exe = exe_dir.join(if cfg!(windows) {
            "index-worker.exe"
        } else {
            "index-worker"
        });
        let mut command = Command::new(exe);
        command
            .arg(database)
            .arg(exe_dir.join("runtime"))
            .arg(if dynamic_fixed { "1" } else { "0" })
            .arg(roots.len().to_string());
        for root in roots {
            command.arg(root);
        }
        for exclusion in exclusions {
            command.arg(exclusion);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().context("无法启动后台索引进程")?;
        #[cfg(windows)]
        let job = match attach_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut input = child.stdin.take().context("无法控制后台索引进程")?;
        let output = child.stdout.take().context("无法读取后台状态")?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(output)
                .lines()
                .map_while(Result::ok)
            {
                if let Ok(value) = serde_json::from_str(&line) {
                    let _ = sender.send(value);
                    wake.request_repaint();
                }
            }
            wake.request_repaint();
        });
        writeln!(input, "start")?;
        input.flush()?;
        Ok(Self {
            child,
            input,
            messages,
            #[cfg(windows)]
            job,
        })
    }

    pub fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()?;
        Ok(())
    }

    pub fn running(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|result| result.is_none())
    }

    pub fn stop(&mut self) {
        let _ = self.send("stop");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while self.running() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if self.running() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(windows)]
fn attach_job(child: &Child) -> Result<windows_sys::Win32::Foundation::HANDLE> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            bail!("无法建立后台资源限制");
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.ProcessMemoryLimit = 512 * 1024 * 1024;
        let cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 1000 },
        };
        let limit_ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of_val(&limits) as u32,
        ) != 0;
        let cpu_ok = SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            &cpu as *const _ as *const _,
            std::mem::size_of_val(&cpu) as u32,
        ) != 0;
        let assigned = AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
        if !limit_ok || !cpu_ok || !assigned {
            CloseHandle(job);
            bail!("无法启用后台 CPU/内存限制，已暂停索引");
        }
        Ok(job)
    }
}
