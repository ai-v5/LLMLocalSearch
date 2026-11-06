use crate::text::{MAX_SOURCE_BYTES, MAX_TEXT_BYTES};
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The packaged parser runs offline; error output is discarded to avoid logging document data.
pub fn extract(runtime: &Path, file: &Path) -> Result<String> {
    if std::fs::metadata(file)?.len() > MAX_SOURCE_BYTES {
        bail!("文件超过解析大小限制");
    }
    let java = runtime.join("java").join("bin").join("java.exe");
    let jar = runtime.join("tika-app.jar");
    let config = runtime.join("tika-config.xml");
    if !java.is_file() || !jar.is_file() || !config.is_file() {
        bail!("文档解析组件不可用");
    }
    let mut command = Command::new(java);
    command
        .args([
            "-Xmx192m",
            "-XX:MaxMetaspaceSize=96m",
            "-XX:ReservedCodeCacheSize=32m",
            "-XX:CompressedClassSpaceSize=32m",
            "-XX:ActiveProcessorCount=2",
            "-XX:+UseSerialGC",
            "-Djava.awt.headless=true",
        ])
        .arg("-jar")
        .arg(jar)
        .arg(format!("--config={}", config.display()))
        .arg("--text")
        .arg("--encoding=UTF-8")
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().context("无法启动文档解析器")?;
    let mut stdout = child.stdout.take().context("无法读取解析结果")?;
    let mut stderr = child.stderr.take().context("无法读取解析状态")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take((MAX_TEXT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let error_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let _ = error_reader.join();
            bail!("文档解析超时");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("无法读取解析结果"))??;
    let error = error_reader.join().unwrap_or_default();
    if bytes.len() > MAX_TEXT_BYTES {
        bail!("提取文本超过大小限制");
    }
    if !status.success() {
        let detail = String::from_utf8_lossy(&error).to_ascii_lowercase();
        if detail.contains("encrypt") || detail.contains("password") {
            bail!("文档已加密");
        }
        bail!("文档解析失败");
    }
    String::from_utf8(bytes).context("文档解析结果编码无效")
}
