// ============================================================================
// uv bundling (WORK-559)
// ============================================================================
// The Pipecat live engine's sidecar is a Python project that Meetily builds
// with `uv sync` into its data folder (src/audio/pipecat.rs). Bundling uv
// makes that work on a machine without uv: Meetily stays one app to launch,
// and the sidecar (and the local docs search it carries) no longer depends on
// a tool the user installed. Without it, such a machine silently fell back to
// the Direct engine.
//
// DO NOT unpin: every archive is checked against the SHA-256 published with
// the uv release, pinned here, so a swapped download fails the build instead
// of shipping. To update, bump UV_VERSION and replace each hash with the
// release's `<archive>.sha256`.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const UV_VERSION: &str = "0.13.9";

/// (target triple, archive name, SHA-256 of the archive), uv 0.13.9.
const ARCHIVES: &[(&str, &str, &str)] = &[
    ("x86_64-unknown-linux-gnu", "uv-x86_64-unknown-linux-gnu.tar.gz", "a7fbaff2b9a271d85c1bdb540941090fd1ce011797b4abeb48e4dcd39c2471d5"),
    ("aarch64-unknown-linux-gnu", "uv-aarch64-unknown-linux-gnu.tar.gz", "0797542018e541a9288855df555c8077eed06f7a47d206362030480e8ecc9f3d"),
    ("x86_64-apple-darwin", "uv-x86_64-apple-darwin.tar.gz", "617a3211261b0fc739562f0071aa34e9739797c510208ede8e2977c30903d041"),
    ("aarch64-apple-darwin", "uv-aarch64-apple-darwin.tar.gz", "5aeca67f2283f75235de40d0bd9c0a5bbc5fce23b6d48b3bf9fa1b2db08382ca"),
    ("x86_64-pc-windows-msvc", "uv-x86_64-pc-windows-msvc.zip", "edd61f7f5728a56cc62455386661860bdfc7bab65872f39b0c25bdcc1d8e1beb"),
];

/// Put `binaries/uv-<target>[.exe]` in place for Tauri's `externalBin`.
pub fn ensure_uv_binary() {
    let target = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .expect("Neither TARGET nor HOST environment variable set");
    let windows = target.contains("windows");
    let name = if windows { format!("uv-{target}.exe") } else { format!("uv-{target}") };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let binaries = PathBuf::from(&manifest_dir).join("binaries");
    let dest = binaries.join(&name);
    let stamp = binaries.join(format!(".{name}.version"));
    if dest.is_file() && std::fs::read_to_string(&stamp).map(|s| s.trim() == UV_VERSION).unwrap_or(false) {
        return;
    }
    let Some((_, archive, sha)) = ARCHIVES.iter().find(|(t, _, _)| *t == target) else {
        panic!("no pinned uv {UV_VERSION} archive for target {target}: add it to build/uv.rs");
    };
    println!("cargo:warning=📥 Bundling uv {UV_VERSION} for {target}");
    std::fs::create_dir_all(&binaries).expect("create binaries directory");
    let url = format!("https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/{archive}");
    let bytes = download(&url).unwrap_or_else(|e| panic!("download uv: {e}"));
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != *sha {
        panic!("uv archive {archive} has SHA-256 {got}, expected {sha}: refusing to bundle it");
    }
    let exe = if windows { "uv.exe" } else { "uv" };
    let binary = if archive.ends_with(".zip") { from_zip(&bytes, exe) } else { from_tar_gz(&bytes, exe) }
        .unwrap_or_else(|e| panic!("extract uv: {e}"));
    write_executable(&dest, &binary).unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    std::fs::write(&stamp, UV_VERSION).expect("write uv version stamp");
    println!("cargo:warning=✅ Bundled uv {UV_VERSION}: {name}");
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// The `uv` executable from a release tarball (`uv-<target>/uv`).
fn from_tar_gz(bytes: &[u8], exe: &str) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path.file_name().and_then(|n| n.to_str()) == Some(exe) {
            let mut out = Vec::new();
            entry.read_to_end(&mut out).map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
    Err(format!("{exe} not found in the archive"))
}

fn from_zip(bytes: &[u8], exe: &str) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        if Path::new(file.name()).file_name().and_then(|n| n.to_str()) == Some(exe) {
            let mut out = Vec::new();
            file.read_to_end(&mut out).map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
    Err(format!("{exe} not found in the archive"))
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(tmp, path)
}
