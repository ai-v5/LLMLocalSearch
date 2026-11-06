use std::path::Path;

#[cfg(windows)]
pub fn within(path: &Path, root: &Path) -> bool {
    fn normal(value: &Path) -> String {
        let mut text = value.to_string_lossy().replace('/', "\\").to_lowercase();
        if let Some(rest) = text.strip_prefix(r"\\?\unc\") {
            text = format!(r"\\{rest}");
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            text = rest.to_owned();
        }
        while text.ends_with('\\') {
            text.pop();
        }
        text
    }
    let path = normal(path);
    let root = normal(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

#[cfg(not(windows))]
pub fn within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(windows)]
pub fn local_volume(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    let value: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume = [0u16; 32768];
    if unsafe { GetVolumePathNameW(value.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0
    {
        return false;
    }
    matches!(unsafe { GetDriveTypeW(volume.as_ptr()) }, 2 | 3 | 6)
}
#[cfg(not(windows))]
pub fn local_volume(_path: &Path) -> bool {
    true
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn windows_case_prefix_and_boundaries() {
        assert!(within(
            Path::new(r"c:\Users\Alice\file.txt"),
            Path::new(r"C:\USERS\alice")
        ));
        assert!(within(
            Path::new(r"C:\Users\Alice\file.txt"),
            Path::new(r"\\?\C:\Users\Alice")
        ));
        assert!(within(Path::new(r"c:\anything"), Path::new("C:\\")));
        assert!(!within(
            Path::new(r"C:\Users\Alice2\file.txt"),
            Path::new(r"C:\Users\Alice")
        ));
    }
}
