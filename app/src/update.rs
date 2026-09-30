//! Self-update from GitHub releases.
//!
//! * Check: the releases list of cyberhirsch/RogueIM (pre-releases included,
//!   since every build so far is an alpha). Newer = higher x.y.z than ours.
//! * Trust: each release carries `SHA256SUMS` and `SHA256SUMS.sig`, an Ed25519
//!   signature by the release key (its public half is compiled in below). The
//!   archive must match its checksum and the checksum file must carry a valid
//!   signature, so neither a mirror nor a tampered download gets installed.
//! * Install: the new files are written next to the running ones and swapped
//!   in by renaming (a running program may be renamed on every OS). The old
//!   ones end in `.old` and are removed at the next start. Then RogueIM
//!   restarts itself.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use sha2::{Digest, Sha256};

const REPO: &str = "cyberhirsch/RogueIM";
/// Ed25519 public key of the release signing key.
const RELEASE_KEY: [u8; 32] = [
    0xd4, 0x2f, 0x11, 0x24, 0xe7, 0x2a, 0x08, 0x59, 0xd4, 0x26, 0x9b, 0xe6, 0x7a, 0xf0, 0x30, 0xe2, 0xe5, 0xc7, 0x18, 0x79, 0x8a, 0xcb, 0xa9, 0x62, 0x4e, 0x61, 0x9b, 0x28, 0xb1, 0x2a, 0xf1, 0x58,
];
/// Refuse downloads larger than this (the archives are ~30-60 MB).
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: (u32, u32, u32),
    archive_url: String,
    archive_name: String,
    sums_url: String,
    sig_url: String,
}

pub enum Progress {
    /// Nothing newer (or checking failed quietly).
    UpToDate,
    Available(Release),
    Step(String),
    /// Installed; restart to use it.
    Ready(String),
    Failed(String),
}

/// "v0.1.4-alpha" -> (0, 1, 4)
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

pub fn current() -> (u32, u32, u32) {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0))
}

/// The release archive built for this platform.
fn archive_name() -> Option<&'static str> {
    if cfg!(windows) {
        Some("RogueIM-windows-x64.zip")
    } else if cfg!(target_os = "macos") {
        Some("RogueIM-macos-universal.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("RogueIM-linux-x64.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("RogueIM-linux-arm64.tar.gz")
    } else {
        None
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(120))).build().into()
}

fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    agent()
        .get(url)
        .header("User-Agent", concat!("RogueIM/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    get(url)?.into_body().into_reader().take(MAX_DOWNLOAD + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() as u64 > MAX_DOWNLOAD {
        return Err("download too large".into());
    }
    Ok(out)
}

/// The newest release that is newer than this build, if any.
pub fn check() -> Result<Option<Release>, String> {
    let name = archive_name().ok_or("no builds for this platform")?;
    let body = fetch(&format!("https://api.github.com/repos/{REPO}/releases?per_page=20"))?;
    let list: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let mut best: Option<Release> = None;
    for r in list.as_array().into_iter().flatten() {
        if r["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        let tag = r["tag_name"].as_str().unwrap_or_default().to_string();
        let Some(version) = parse_version(&tag) else { continue };
        if version <= current() || best.as_ref().map(|b| b.version >= version).unwrap_or(false) {
            continue;
        }
        let asset = |n: &str| r["assets"].as_array().into_iter().flatten().find(|a| a["name"] == n).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
        // Unsigned releases are never offered.
        let (Some(archive_url), Some(sums_url), Some(sig_url)) = (asset(name), asset("SHA256SUMS"), asset("SHA256SUMS.sig")) else { continue };
        best = Some(Release { tag, version, archive_url, archive_name: name.to_string(), sums_url, sig_url });
    }
    Ok(best)
}

/// Check the signature of the checksum file, then the archive's checksum.
fn verify(archive: &[u8], name: &str, sums: &[u8], sig: &[u8]) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key = VerifyingKey::from_bytes(&RELEASE_KEY).map_err(|e| e.to_string())?;
    let sig = Signature::from_slice(sig).map_err(|_| "bad signature file".to_string())?;
    key.verify(sums, &sig).map_err(|_| "the release signature is not valid".to_string())?;
    let want = String::from_utf8_lossy(sums)
        .lines()
        .find_map(|l| {
            let (hash, file) = l.split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then(|| hash.to_lowercase())
        })
        .ok_or("the archive is not listed in SHA256SUMS")?;
    if hex::encode(Sha256::digest(archive)) != want {
        return Err("the download does not match its checksum".into());
    }
    Ok(())
}

/// Files of an archive: (file name without the top folder, contents).
fn unpack(name: &str, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let strip = |p: &str| p.split_once('/').map(|(_, rest)| rest.to_string()).unwrap_or_default();
    let mut out = vec![];
    if name.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            if f.is_dir() {
                continue;
            }
            let path = strip(&f.name().replace('\\', "/"));
            let mut buf = vec![];
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            out.push((path, buf));
        }
    } else {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(data));
        for e in t.entries().map_err(|e| e.to_string())? {
            let mut e = e.map_err(|e| e.to_string())?;
            if !e.header().entry_type().is_file() {
                continue;
            }
            let path = strip(&e.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/"));
            let mut buf = vec![];
            e.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            out.push((path, buf));
        }
    }
    // Only plain relative paths: nothing may escape the install folder.
    out.retain(|(p, _)| !p.is_empty() && !p.starts_with('/') && !p.split('/').any(|c| c == ".." || c.contains(':')));
    Ok(out)
}

/// Where the running program lives, and (macOS) the app bundle around it.
fn install_dir() -> Result<(PathBuf, Option<PathBuf>), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no install folder")?.to_path_buf();
    let bundle = dir.ancestors().find(|p| p.extension().map(|e| e == "app").unwrap_or(false)).map(Path::to_path_buf);
    Ok((dir, bundle))
}

/// Swap one file in: write `name.new`, move the current one to `name.old`,
/// move the new one into place.
fn replace(dir: &Path, rel: &str, bytes: &[u8], exec: bool) -> Result<(), String> {
    let target = dir.join(rel);
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let new = target.with_extension(format!("{}new", target.extension().map(|e| format!("{}.", e.to_string_lossy())).unwrap_or_default()));
    let old = target.with_extension(format!("{}old", target.extension().map(|e| format!("{}.", e.to_string_lossy())).unwrap_or_default()));
    std::fs::write(&new, bytes).map_err(|e| format!("{}: {e}", new.display()))?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755));
    }
    let _ = exec;
    let _ = std::fs::remove_file(&old);
    if target.exists() {
        std::fs::rename(&target, &old).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    std::fs::rename(&new, &target).map_err(|e| {
        let _ = std::fs::rename(&old, &target);
        format!("{}: {e}", target.display())
    })
}

fn install(r: &Release, report: &dyn Fn(Progress)) -> Result<(), String> {
    report(Progress::Step(format!("downloading {}…", r.tag)));
    let archive = fetch(&r.archive_url)?;
    let sums = fetch(&r.sums_url)?;
    let sig = fetch(&r.sig_url)?;
    report(Progress::Step("checking the signature…".into()));
    verify(&archive, &r.archive_name, &sums, &sig)?;
    report(Progress::Step("installing…".into()));
    let files = unpack(&r.archive_name, &archive)?;
    let (dir, bundle) = install_dir()?;
    let bins = ["rogueim", "rim-cli", "rim-plugin-pomodoro", "rim-plugin-todo", "rim-plugin-player"];
    let is_bin = |p: &str| bins.iter().any(|b| p == *b || p == format!("{b}.exe"));
    if !files.iter().any(|(p, _)| p == "rogueim" || p == "rogueim.exe") {
        return Err("the archive holds no RogueIM program".into());
    }
    for (path, bytes) in &files {
        // Inside a macOS bundle only the programs are replaced (sounds and
        // docs live beside the bundle in the disk image, not in it).
        if bundle.is_some() && !is_bin(path) {
            continue;
        }
        replace(&dir, path, bytes, is_bin(path))?;
    }
    // The bundle's ad-hoc signature covers the programs: renew it.
    if let Some(b) = bundle {
        let ok = std::process::Command::new("codesign").args(["--force", "--sign", "-"]).arg(&b).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return Err("could not re-sign RogueIM.app".into());
        }
    }
    Ok(())
}

/// Start a check, and if `auto`, install what it finds. Progress arrives on
/// the returned channel.
pub fn start(auto: bool) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || match check() {
        Ok(Some(r)) if auto => {
            let tag = r.tag.clone();
            let t2 = tx.clone();
            match install(&r, &move |p| {
                let _ = t2.send(p);
            }) {
                Ok(()) => {
                    let _ = tx.send(Progress::Ready(tag));
                }
                Err(e) => {
                    let _ = tx.send(Progress::Failed(e));
                }
            }
        }
        Ok(Some(r)) => {
            let _ = tx.send(Progress::Available(r));
        }
        Ok(None) => {
            let _ = tx.send(Progress::UpToDate);
        }
        Err(e) => {
            let _ = tx.send(Progress::Failed(format!("update check: {e}")));
        }
    });
    rx
}

/// Install a release found earlier.
pub fn install_now(r: Release) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let tag = r.tag.clone();
        let t2 = tx.clone();
        let res = install(&r, &move |p| {
            let _ = t2.send(p);
        });
        let _ = tx.send(match res {
            Ok(()) => Progress::Ready(tag),
            Err(e) => Progress::Failed(e),
        });
    });
    rx
}

/// Remove files left over from the last update.
pub fn clean_up() {
    let Ok((dir, _)) = install_dir() else { return };
    for base in [dir.clone(), dir.join("sounds")] {
        for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".old") || n.ends_with(".new") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Start the freshly installed RogueIM with the same arguments. It waits for
/// this process to finish before touching the profile.
pub fn restart() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--after-update").collect();
    args.push("--after-update".into());
    let _ = std::process::Command::new(exe).args(args).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.1.4-alpha"), Some((0, 1, 4)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert!(parse_version("v0.1.10").unwrap() > parse_version("v0.1.9").unwrap());
    }

    #[test]
    fn rejects_bad_signature_and_checksum() {
        let archive = b"archive bytes";
        let sums = format!("{}  RogueIM-windows-x64.zip\n", hex::encode(Sha256::digest(archive)));
        assert!(verify(archive, "RogueIM-windows-x64.zip", sums.as_bytes(), &[0u8; 64]).is_err());
    }

    #[test]
    fn unpack_strips_top_folder_and_blocks_escapes() {
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("RogueIM-windows-x64/rogueim.exe", o).unwrap();
            std::io::Write::write_all(&mut z, b"exe").unwrap();
            z.start_file("RogueIM-windows-x64/../evil.exe", o).unwrap();
            std::io::Write::write_all(&mut z, b"x").unwrap();
            z.start_file("RogueIM-windows-x64/sounds/message.wav", o).unwrap();
            std::io::Write::write_all(&mut z, b"wav").unwrap();
            z.finish().unwrap();
        }
        let files = unpack("x.zip", buf.get_ref()).unwrap();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, ["rogueim.exe", "sounds/message.wav"]);
    }
}

#[cfg(test)]
mod signed {
    /// With RIM_TEST_SIGNED=<dir> holding archive.zip, SHA256SUMS and
    /// SHA256SUMS.sig made like in CI, the real verification must accept them.
    #[test]
    fn accepts_ci_signature() {
        let Some(d) = std::env::var_os("RIM_TEST_SIGNED") else { return };
        let d = std::path::Path::new(&d);
        let r = |n: &str| std::fs::read(d.join(n)).unwrap();
        super::verify(&r("archive.zip"), "archive.zip", &r("SHA256SUMS"), &r("SHA256SUMS.sig")).unwrap();
        // one flipped byte in the archive must fail
        let mut bad = r("archive.zip");
        bad[0] ^= 1;
        assert!(super::verify(&bad, "archive.zip", &r("SHA256SUMS"), &r("SHA256SUMS.sig")).is_err());
    }
}
