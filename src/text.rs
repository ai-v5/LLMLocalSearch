use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Read;
use std::path::Path;

// Oversized files are reported explicitly instead of silently indexing a prefix.
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

pub fn read_plain(path: &Path) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let mut file = options.open(path).context("无法读取文件")?;
    let metadata = file.metadata().context("无法读取文件")?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("链接文件不解析正文");
        }
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("文件超过解析大小限制");
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("无法读取文件")?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        bail!("文件超过解析大小限制");
    }
    let text = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        std::str::from_utf8(&bytes[3..])
            .context("UTF-8 文件编码无效")?
            .to_owned()
    } else if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let encoding = if bytes[0] == 0xff {
            encoding_rs::UTF_16LE
        } else {
            encoding_rs::UTF_16BE
        };
        let (decoded, invalid) = encoding.decode_without_bom_handling(&bytes[2..]);
        if invalid {
            bail!("UTF-16 文件编码无效");
        }
        decoded.into_owned()
    } else if let Ok(text) = std::str::from_utf8(&bytes) {
        text.to_owned()
    } else {
        let (decoded, invalid) = encoding_rs::GB18030.decode_without_bom_handling(&bytes);
        if invalid {
            bail!("无法识别文件编码");
        }
        decoded.into_owned()
    };
    if text.len() > MAX_TEXT_BYTES {
        bail!("提取文本超过大小限制");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reads_utf8_utf16_and_gb18030() -> Result<()> {
        let temp = tempdir()?;
        let p = temp.path().join("sample.txt");
        fs::write(&p, "文本搜索")?;
        assert_eq!(read_plain(&p)?, "文本搜索");
        let mut utf16 = vec![0xff, 0xfe];
        for c in "文本搜索".encode_utf16() {
            utf16.extend_from_slice(&c.to_le_bytes());
        }
        fs::write(&p, utf16)?;
        assert_eq!(read_plain(&p)?, "文本搜索");
        let (encoded, _, _) = encoding_rs::GB18030.encode("文本搜索");
        fs::write(&p, encoded.as_ref())?;
        assert_eq!(read_plain(&p)?, "文本搜索");
        Ok(())
    }
}
