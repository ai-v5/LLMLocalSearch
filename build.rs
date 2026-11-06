fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/windows.rc");
    #[cfg(windows)]
    {
        let target = std::env::var("TARGET").expect("TARGET");
        if target.contains("msvc") {
            let rc = cc::windows_registry::find_tool(&target, "rc.exe")
                .expect("Windows SDK resource compiler is required");
            let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
                .join("app-icon.res");
            let status = rc
                .to_command()
                .arg("/nologo")
                .arg(format!("/fo{}", output.display()))
                .arg("assets/windows.rc")
                .status()
                .expect("start rc.exe");
            assert!(status.success(), "failed to compile application icon");
            println!(
                "cargo:rustc-link-arg-bin=llmlocalsearch={}",
                output.display()
            );
        }
    }
}
