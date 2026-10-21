// Shared by the two executable crates. Windows SDK rc.exe is a build prerequisite.
fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/version-resource.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    use std::{env, fs, path::PathBuf, process::Command};
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let numeric = format!("{},{},{},{}", env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        env::var("CARGO_PKG_VERSION_MINOR").unwrap(), env::var("CARGO_PKG_VERSION_PATCH").unwrap(),
        version.rsplit_once("rc.").map_or("0", |(_, n)| n));
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let rc = out.join("sortsize-version.rc");
    let res = out.join("sortsize-version.res");
    let flags = if version.contains('-') { 2 } else { 0 };
    fs::write(&rc, format!(r#"#pragma code_page(65001)
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS {flags}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904B0"
  BEGIN
   VALUE "FileDescription", "SortSize storage analysis\0"
   VALUE "FileVersion", "{version}\0"
   VALUE "ProductName", "SortSize\0"
   VALUE "ProductVersion", "{version}\0"
   VALUE "LegalCopyright", "SortSize contributors; GPL-3.0-only\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#)).expect("write version resource");
    // A Developer PowerShell supplies rc.exe. Cargo/rustup may also run from SSH;
    // resolve the installed SDK without changing the machine's PATH.
    let compiler = if Command::new("rc.exe").arg("/?").output().is_ok() {
        PathBuf::from("rc.exe")
    } else {
        let kits = PathBuf::from(env::var_os("ProgramFiles(x86)").expect("Program Files x86"))
            .join("Windows Kits/10/bin");
        let mut candidates: Vec<_> = fs::read_dir(kits).expect("Windows SDK installed")
            .filter_map(Result::ok).map(|e| e.path().join("x64/rc.exe"))
            .filter(|p| p.is_file()).collect();
        candidates.sort();
        candidates.pop().expect("Windows SDK rc.exe required")
    };
    let status = Command::new(compiler).arg("/nologo").arg("/fo").arg(&res).arg(&rc)
        .status().expect("run Windows resource compiler");
    assert!(status.success(), "Windows version resource compilation failed");
    println!("cargo:rustc-link-arg-bin={}={}",
        if env::var("CARGO_PKG_NAME").unwrap() == "sortsize-cli" { "sortsize" } else { "sortsize-slint-spike" },
        res.display());
}
