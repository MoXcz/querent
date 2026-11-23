//! Checks GitHub for newer releases and installs a signed one when asked.

use std::io::{Cursor, Read as _, Write as _};
use std::path::{Component, Path};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Global, Task, Window, actions};
use serde::Deserialize;

use crate::tokio_bridge;

actions!(
    querent,
    [
        /// Ask GitHub whether a newer Querent is out.
        CheckForUpdates
    ]
);

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/MoXcz/querent/releases/latest";
/// Releases are signed in CI with the matching secret key.
const PUBLIC_KEY: &str = include_str!("../minisign.pub");
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// Far above a release archive; a bigger answer is refused before it fills memory.
const MAX_DOWNLOAD_BYTES: u64 = 300 * 1024 * 1024;
const BINARY_NAME: &str = if cfg!(windows) {
    "querent.exe"
} else {
    "querent"
};

/// Identifies the update notification, so each step replaces the last.
struct UpdateNotice;

#[derive(Default)]
struct Updater {
    check: Option<Task<()>>,
    install: Option<Task<()>>,
    /// A second install, or a check, waits for this one to end.
    installing: bool,
}

impl Global for Updater {}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
struct Update {
    version: semver::Version,
    page_url: String,
    /// Missing when the release has no signed build for this platform.
    download: Option<Download>,
}

#[derive(Debug, Clone)]
struct Download {
    archive_name: String,
    archive_url: String,
    signature_url: String,
}

pub fn init(cx: &mut App) {
    cx.set_global(Updater::default());
}

/// Checks once at startup, unless turned off or running a development build.
pub fn check_on_start(window: &mut Window, cx: &mut App) {
    if cfg!(debug_assertions) || !crate::settings::get(cx).check_for_updates {
        return;
    }
    check(false, window, cx);
}

/// Looks for a newer release; `manual` also reports "up to date" and errors.
pub fn check(manual: bool, window: &mut Window, cx: &mut App) {
    if cx.global::<Updater>().installing {
        if manual {
            notify(
                window,
                cx,
                Notification::info("An update is already installing."),
            );
        }
        return;
    }
    let request = tokio_bridge::spawn(cx, async {
        let release = latest_release().await?;
        // Probing the program's folder touches the disk, so it stays off the UI thread.
        let blocked = tokio::task::spawn_blocking(|| match std::env::current_exe() {
            Ok(exe) => replace_blocker(&exe),
            Err(error) => Some(format!("Cannot find the running program: {error}")),
        })
        .await
        .context("the update check stopped")?;
        anyhow::Ok((release, blocked))
    });
    let task = window.spawn(cx, async move |cx| {
        let result = request.await.and_then(|result| result);
        cx.update(|window, cx| match result {
            Ok((release, blocked)) => match newer_update(&release) {
                Some(update) => show_available(update, blocked, window, cx),
                None if manual => notify(
                    window,
                    cx,
                    Notification::success(format!(
                        "Querent {CURRENT_VERSION} is the newest version."
                    )),
                ),
                None => {}
            },
            Err(error) => {
                tracing::warn!(%error, "update check failed");
                if manual {
                    notify(
                        window,
                        cx,
                        Notification::error(format!("Could not check for updates: {error:#}")),
                    );
                }
            }
        })
        .ok();
    });
    cx.global_mut::<Updater>().check = Some(task);
}

fn notify(window: &mut Window, cx: &mut App, note: Notification) {
    window.remove_notification::<UpdateNotice>(cx);
    window.push_notification(note.title("Update").id::<UpdateNotice>(), cx);
}

fn show_available(update: Update, blocked: Option<String>, window: &mut Window, cx: &mut App) {
    if cx.global::<Updater>().installing {
        return;
    }
    let message = format!(
        "Querent {} is available; you have {CURRENT_VERSION}. Click here for what's new.",
        update.version
    );
    let page_url = update.page_url.clone();
    let blocked = blocked.map_or(Ok(()), Err);
    let note = Notification::info(message)
        .autohide(false)
        .on_click(move |_, _, cx| cx.open_url(&page_url));
    let note = match (blocked, update.download.clone()) {
        (Ok(()), Some(download)) => note.action(move |_, _, _| {
            let update = update.clone();
            let download = download.clone();
            Button::new("install-update")
                .primary()
                .label("Update")
                .on_click(move |_, window, cx| {
                    install(update.clone(), download.clone(), window, cx);
                })
        }),
        (Err(reason), _) => note.message(format!(
            "Querent {} is available; you have {CURRENT_VERSION}. {reason} Click here to download it.",
            update.version
        )),
        (Ok(()), None) => note.message(format!(
            "Querent {} is available; you have {CURRENT_VERSION}. There is no build for this system; click here for the release.",
            update.version
        )),
    };
    notify(window, cx, note);
}

fn install(update: Update, download: Download, window: &mut Window, cx: &mut App) {
    if cx.global::<Updater>().installing {
        return;
    }
    notify(
        window,
        cx,
        Notification::info(format!("Downloading Querent {}…", update.version)).autohide(false),
    );
    let work = tokio_bridge::spawn(cx, async move {
        let client = client(DOWNLOAD_TIMEOUT)?;
        let archive = fetch(&client, &download.archive_url).await?;
        let signature = String::from_utf8(fetch(&client, &download.signature_url).await?)
            .context("the signature is not text")?;
        tokio::task::spawn_blocking(move || {
            verify(&archive, &signature, &download.archive_name)?;
            let binary = extract(&archive, &download.archive_name)?;
            replace_running_binary(&binary)
        })
        .await
        .context("the install task stopped")?
    });
    let version = update.version;
    let task = window.spawn(cx, async move |cx| {
        let result = work.await.and_then(|result| result);
        cx.update(|window, cx| {
            cx.global_mut::<Updater>().installing = false;
            let note = match result {
                Ok(()) => Notification::success(format!(
                    "Querent {version} is installed. Restart Querent to use it."
                ))
                .autohide(false),
                Err(error) => {
                    tracing::warn!(%error, "update install failed");
                    Notification::error(format!("Could not install the update: {error:#}"))
                        .autohide(false)
                }
            };
            notify(window, cx, note);
        })
        .ok();
    });
    let updater = cx.global_mut::<Updater>();
    updater.install = Some(task);
    updater.installing = true;
}

fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("querent/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CHECK_TIMEOUT)
        .timeout(timeout)
        .build()
        .context("building the HTTP client")
}

async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .context("downloading")?
        .error_for_status()
        .context("downloading")?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("downloading")? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > MAX_DOWNLOAD_BYTES {
            bail!("the download is larger than any release");
        }
    }
    Ok(body)
}

async fn latest_release() -> anyhow::Result<Release> {
    let client = client(CHECK_TIMEOUT)?;
    client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?
        .error_for_status()
        .context("asking GitHub for the latest release")?
        .json()
        .await
        .context("reading GitHub's answer")
}

/// The release, when it is newer than this build.
fn newer_update(release: &Release) -> Option<Update> {
    let version = semver::Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
    let current = semver::Version::parse(CURRENT_VERSION).ok()?;
    if version <= current {
        return None;
    }
    let download = archive_name(&release.tag_name).and_then(|archive_name| {
        let url_of = |name: &str| {
            release
                .assets
                .iter()
                .find(|asset| asset.name == name)
                .map(|asset| asset.browser_download_url.clone())
        };
        Some(Download {
            archive_url: url_of(&archive_name)?,
            signature_url: url_of(&format!("{archive_name}.minisig"))?,
            archive_name,
        })
    });
    Some(Update {
        version,
        page_url: release.html_url.clone(),
        download,
    })
}

/// The release archive for this platform, as the release workflow names it.
fn archive_name(tag: &str) -> Option<String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(format!("querent-{tag}-x86_64-linux.tar.gz")),
        ("windows", "x86_64") => Some(format!("querent-{tag}-x86_64-windows.zip")),
        _ => None,
    }
}

/// The trusted comment must name this archive, so another signed release is refused.
fn verify(archive: &[u8], signature: &str, archive_name: &str) -> anyhow::Result<()> {
    verify_with(PUBLIC_KEY, archive, signature, archive_name)
}

fn verify_with(
    public_key: &str,
    archive: &[u8],
    signature: &str,
    archive_name: &str,
) -> anyhow::Result<()> {
    let key = minisign_verify::PublicKey::decode(public_key).context("reading the public key")?;
    let signature =
        minisign_verify::Signature::decode(signature).context("reading the signature")?;
    key.verify(archive, &signature, false)
        .context("the download's signature does not match")?;
    if signature.trusted_comment() != archive_name {
        bail!("the signature is for another file");
    }
    Ok(())
}

/// The `querent` binary inside a release `.tar.gz` or `.zip`.
fn extract(archive: &[u8], archive_name: &str) -> anyhow::Result<Vec<u8>> {
    let is_binary = |path: &Path| path.file_name().is_some_and(|name| name == BINARY_NAME);
    let mut binary = Vec::new();
    let is_zip = Path::new(archive_name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
    if is_zip {
        let mut zip = zip::ZipArchive::new(Cursor::new(archive)).context("opening the zip")?;
        for index in 0..zip.len() {
            let entry = zip.by_index(index).context("reading the zip")?;
            if entry.is_file() && entry.enclosed_name().is_some_and(|path| is_binary(&path)) {
                entry
                    .take(MAX_DOWNLOAD_BYTES)
                    .read_to_end(&mut binary)
                    .context("unpacking")?;
                break;
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries().context("opening the archive")? {
            let entry = entry.context("reading the archive")?;
            if entry.header().entry_type().is_file()
                && entry.path().is_ok_and(|path| is_binary(&path))
            {
                entry
                    .take(MAX_DOWNLOAD_BYTES)
                    .read_to_end(&mut binary)
                    .context("unpacking")?;
                break;
            }
        }
    }
    if binary.is_empty() {
        return Err(anyhow!("the download has no {BINARY_NAME}"));
    }
    Ok(binary)
}

/// Why this copy cannot replace itself, if it cannot.
fn replace_blocker(exe: &Path) -> Option<String> {
    let parts: Vec<Component<'_>> = exe.components().collect();
    let is = |component: &Component<'_>, name: &str| *component == Component::Normal(name.as_ref());
    if parts.iter().any(|part| is(part, ".cargo")) {
        return Some("This copy was installed with cargo; run `cargo install` again.".to_owned());
    }
    let built_here = parts
        .windows(2)
        .any(|pair| is(&pair[0], "target") && (is(&pair[1], "release") || is(&pair[1], "debug")));
    if built_here {
        return Some("This is a development build.".to_owned());
    }
    let directory = exe.parent()?;
    match create_new_file(directory, "check") {
        Ok((probe, _)) => {
            if let Err(error) = std::fs::remove_file(&probe) {
                tracing::debug!(%error, "could not remove the update probe file");
            }
            None
        }
        Err(_) => Some(format!(
            "Querent cannot write to {}, so it cannot update itself.",
            directory.display()
        )),
    }
}

/// A new file in `directory` that did not exist before, so nothing another
/// user placed there (such as a symlink) is written through.
fn create_new_file(
    directory: &Path,
    purpose: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = directory.join(format!(
        ".querent-update-{purpose}-{}-{stamp}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o755);
    }
    let file = options.open(&path)?;
    Ok((path, file))
}

fn replace_running_binary(binary: &[u8]) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("finding the running program")?;
    let directory = exe.parent().context("finding the program's folder")?;
    let (staged, mut file) = create_new_file(directory, "new").context("saving the new version")?;
    let written = file
        .write_all(binary)
        .and_then(|()| file.sync_all())
        .context("saving the new version");
    drop(file);
    let replaced =
        written.and_then(|()| self_replace::self_replace(&staged).context("replacing the program"));
    if let Err(error) = std::fs::remove_file(&staged) {
        tracing::debug!(%error, "could not remove the staged update");
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.to_owned(),
            html_url: "https://example.com/release".to_owned(),
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).to_owned(),
                    browser_download_url: format!("https://example.com/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn offers_only_newer_releases() {
        assert!(newer_update(&release(&format!("v{CURRENT_VERSION}"), &[])).is_none());
        assert!(newer_update(&release("v0.0.1", &[])).is_none());
        assert!(newer_update(&release("not-a-version", &[])).is_none());
        let update = newer_update(&release("v999.0.0", &[])).unwrap();
        assert_eq!(update.version, semver::Version::new(999, 0, 0));
        assert!(update.download.is_none(), "no assets, nothing to download");
    }

    #[test]
    fn picks_this_platforms_archive_and_signature() {
        let Some(name) = archive_name("v999.0.0") else {
            return;
        };
        let signature = format!("{name}.minisig");
        let update = newer_update(&release("v999.0.0", &[&name, &signature])).unwrap();
        let download = update.download.unwrap();
        assert_eq!(download.archive_name, name);
        assert!(download.signature_url.ends_with(".minisig"));
        assert!(
            newer_update(&release("v999.0.0", &[&name]))
                .unwrap()
                .download
                .is_none()
        );
    }

    /// A throwaway key, not the release key, with a signature made by
    /// `rsign sign -W -t querent-test.tar.gz` over the bytes `hello\n`.
    const TEST_KEY: &str = "untrusted comment: minisign public key: 38D120078C532C8C\nRWSMLFOMByDROPtj0lXMmo/wdc2efXjrYmfb8i83jQvJ1IwvF9plXvJZ\n";
    const TEST_SIGNATURE: &str = "untrusted comment: signature from rsign secret key\nRUSMLFOMByDROCvf9zfwiUM9h5kJyjacLMLX9VuJEnwQ42pmsvzf8ZwG2uCrxxlk8OZVSwFBHF2VEA1hnnqxzcnf6L15Wb72sAg=\ntrusted comment: querent-test.tar.gz\nfR9zdSHPufbIhXmgck3uyODgGppHkhAfmwQU3QpptL34OAW1xl5lWf2rFzPulX/ElhBY713xZd8V56n/Kf3KBA==\n";

    #[test]
    fn the_built_in_key_parses() {
        assert!(minisign_verify::PublicKey::decode(PUBLIC_KEY).is_ok());
    }

    #[test]
    fn checks_the_signature_and_file_name() {
        let name = "querent-test.tar.gz";
        assert!(verify_with(TEST_KEY, b"hello\n", TEST_SIGNATURE, name).is_ok());
        assert!(verify_with(TEST_KEY, b"tampered\n", TEST_SIGNATURE, name).is_err());
        assert!(verify_with(TEST_KEY, b"hello\n", TEST_SIGNATURE, "other.tar.gz").is_err());
        assert!(verify_with(TEST_KEY, b"hello\n", "garbage", name).is_err());
        // The release key did not make this signature.
        assert!(verify(b"hello\n", TEST_SIGNATURE, name).is_err());
    }

    #[test]
    fn extracts_the_binary_from_both_archive_kinds() {
        let mut tar_bytes = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            for (path, body) in [("q/README.md", &b"readme"[..]), ("q/querent", b"elf")] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_cksum();
                builder.append_data(&mut header, path, body).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            writer
                .start_file("q/querent.exe", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"exe").unwrap();
            writer.finish().unwrap();
        }
        let expected: &[u8] = if cfg!(windows) { b"exe" } else { b"elf" };
        let (archive, name) = if cfg!(windows) {
            (&zip_bytes, "a.zip")
        } else {
            (&tar_bytes, "a.tar.gz")
        };
        assert_eq!(extract(archive, name).unwrap(), expected);
        // The other kind holds the other platform's name, so it is not found.
        let (other, other_name) = if cfg!(windows) {
            (&tar_bytes, "a.tar.gz")
        } else {
            (&zip_bytes, "a.zip")
        };
        assert!(extract(other, other_name).is_err());
    }

    #[test]
    fn cargo_and_development_builds_do_not_replace_themselves() {
        assert!(replace_blocker(Path::new("/home/me/.cargo/bin/querent")).is_some());
        assert!(replace_blocker(Path::new("/src/querent/target/release/querent")).is_some());
        let elsewhere = std::env::temp_dir().join("target").join("querent");
        assert_ne!(
            replace_blocker(&elsewhere).as_deref(),
            Some("This is a development build.")
        );
    }
}
