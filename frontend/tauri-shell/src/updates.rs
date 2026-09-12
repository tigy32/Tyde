use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use host_config::updates::{
    AppUpdateStatus, UpdateChannel, UpdateDismissal, UpdatePhase, UpdatePreferences,
};
use semver::Version;
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

const CHECK_INTERVAL: u64 = 6 * 60 * 60;
const REMINDER_INTERVAL: u64 = 24 * 60 * 60;
const RELEASES: &str = "https://api.github.com/repos/tigy32/Tyde/releases";
const DOWNLOADS: &str = "https://github.com/tigy32/Tyde/releases/download";

struct Inner {
    status: AppUpdateStatus,
    pending: Option<Update>,
    next_check: u64,
    seen_servers: std::collections::HashSet<String>,
}

struct Updates {
    inner: Mutex<Inner>,
    operation: tokio::sync::Mutex<()>,
    preferences_path: PathBuf,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Updates {
    fn change(&self, app: &AppHandle, change: impl FnOnce(&mut Inner)) -> AppUpdateStatus {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        change(&mut inner);
        inner.status.revision += 1;
        let status = inner.status.clone();
        if let Err(error) = app.emit("tyde://app-update", &status) {
            tracing::warn!(%error, "could not broadcast app update status");
        }
        status
    }

    fn snapshot(&self) -> AppUpdateStatus {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .status
            .clone()
    }

    fn save(&self, preferences: &UpdatePreferences) -> Result<(), String> {
        use std::io::Write;
        let parent = self
            .preferences_path
            .parent()
            .ok_or("Update preferences have no directory")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temporary = self.preferences_path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&serde_json::to_vec(preferences).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(temporary, &self.preferences_path).map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

async fn release_endpoint(
    channel: UpdateChannel,
    current: &Version,
    releases_url: &str,
    downloads_url: &str,
) -> Result<Option<(Version, tauri::Url)>, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("Tyde/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let mut newest: Option<Version> = None;
    for page in 1..=20 {
        let releases: Vec<Release> = client
            .get(format!("{releases_url}?per_page=100&page={page}"))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())?;
        let last_page = releases.len() < 100;
        for release in releases {
            let Ok(version) = Version::parse(
                release
                    .tag_name
                    .strip_prefix('v')
                    .unwrap_or(&release.tag_name),
            ) else {
                continue;
            };
            if release.draft
                || (channel == UpdateChannel::Release
                    && (release.prerelease || !version.pre.is_empty()))
                || version <= *current
                || newest.as_ref().is_some_and(|previous| version <= *previous)
                || !release
                    .assets
                    .iter()
                    .any(|asset| asset.name == "tyde-update.json")
            {
                continue;
            }
            newest = Some(version);
        }
        if last_page {
            return newest
                .map(|version| {
                    let url =
                        tauri::Url::parse(&format!("{downloads_url}/v{version}/tyde-update.json"))
                            .map_err(|error| error.to_string());
                    url.map(|url| (version, url))
                })
                .transpose();
        }
    }
    Err("The release catalog is too large to finish checking. Please try again later.".into())
}

async fn check(app: &AppHandle, manual: bool) -> Result<AppUpdateStatus, String> {
    let updates = app.state::<Updates>();
    let Ok(_operation) = updates.operation.try_lock() else {
        return Ok(updates.snapshot());
    };
    let before = updates.snapshot();
    if !manual && !before.preferences.automatic {
        return Ok(before);
    }
    updates.change(app, |inner| {
        inner.status.phase = UpdatePhase::Checking;
        inner.status.error = None;
        inner.status.prompt = false;
    });
    let result = async {
        let current = app.package_info().version.clone();
        let Some((version, endpoint)) =
            release_endpoint(before.preferences.channel, &current, RELEASES, DOWNLOADS).await?
        else {
            return Ok(None);
        };
        let shutdown_app = app.clone();
        let updater = app
            .updater_builder()
            .endpoints(vec![endpoint])
            .map_err(|error| error.to_string())?
            .timeout(Duration::from_secs(30))
            .on_before_exit(move || crate::shutdown_managed_host(&shutdown_app))
            .build()
            .map_err(|error| error.to_string())?;
        let mut update = updater.check().await.map_err(|error| error.to_string())?;
        if let Some(update) = &mut update {
            if update.version != version.to_string()
                || !update
                    .download_url
                    .as_str()
                    .starts_with(&format!("{DOWNLOADS}/v{version}/"))
            {
                return Err("The update manifest does not match the selected release.".to_owned());
            }
            update.timeout = Some(Duration::from_secs(15 * 60));
        }
        Ok::<_, String>(update)
    }
    .await;
    let status = updates.change(app, |inner| {
        inner.next_check = now()
            + if result.is_ok() {
                CHECK_INTERVAL
            } else {
                60 * 60
            };
        match result {
            Ok(update) => {
                inner.status.last_checked = Some(now());
                inner.status.version = update.as_ref().map(|update| update.version.clone());
                inner.status.notes = update.as_ref().and_then(|update| update.body.clone());
                inner.status.phase = if update.is_some() {
                    UpdatePhase::Available
                } else {
                    UpdatePhase::Idle
                };
                inner.status.prompt =
                    update.is_some() && (manual || now() >= inner.status.preferences.remind_after);
                inner.pending = update;
            }
            Err(error) => {
                tracing::warn!(%error, "app update check failed");
                inner.status.phase = UpdatePhase::Error;
                inner.status.error = Some(error);
            }
        }
    });
    Ok(status)
}

#[tauri::command]
fn status(app: AppHandle) -> AppUpdateStatus {
    app.state::<Updates>().snapshot()
}

#[tauri::command]
async fn check_now(app: AppHandle) -> Result<AppUpdateStatus, String> {
    check(&app, true).await
}

#[tauri::command]
async fn configure(
    app: AppHandle,
    channel: UpdateChannel,
    automatic: bool,
) -> Result<AppUpdateStatus, String> {
    {
        let updates = app.state::<Updates>();
        let _operation = updates
            .operation
            .try_lock()
            .map_err(|_| "An update operation is in progress")?;
        let preferences = UpdatePreferences {
            channel,
            automatic,
            remind_after: 0,
        };
        updates.save(&preferences)?;
        updates.change(&app, |inner| {
            inner.status.preferences = preferences;
            inner.status.phase = UpdatePhase::Idle;
            inner.status.version = None;
            inner.status.notes = None;
            inner.status.prompt = false;
            inner.status.error = None;
            inner.pending = None;
            inner.seen_servers.clear();
            inner.next_check = 0;
        });
    }
    check(&app, false).await
}

#[tauri::command]
async fn dismiss(app: AppHandle, choice: UpdateDismissal) -> Result<AppUpdateStatus, String> {
    let updates = app.state::<Updates>();
    let _operation = updates
        .operation
        .try_lock()
        .map_err(|_| "An update operation is in progress")?;
    let mut preferences = updates.snapshot().preferences;
    match choice {
        UpdateDismissal::Never => preferences.automatic = false,
        UpdateDismissal::NotNow => preferences.remind_after = now() + REMINDER_INTERVAL,
    }
    updates.save(&preferences)?;
    Ok(updates.change(&app, |inner| {
        inner.status.preferences = preferences;
        inner.status.prompt = false;
    }))
}

#[tauri::command]
async fn server_version(app: AppHandle, version: String) -> Result<AppUpdateStatus, String> {
    let version =
        Version::parse(version.trim_start_matches('v')).map_err(|error| error.to_string())?;
    if version <= app.package_info().version {
        return Ok(app.state::<Updates>().snapshot());
    }
    let should_check = {
        let updates = app.state::<Updates>();
        let mut inner = updates
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if inner.status.phase.busy() || !inner.status.preferences.automatic {
            false
        } else {
            inner.seen_servers.insert(version.to_string())
        }
    };
    if should_check {
        check(&app, false).await
    } else {
        Ok(app.state::<Updates>().snapshot())
    }
}

#[tauri::command]
async fn install(app: AppHandle, version: String) -> Result<AppUpdateStatus, String> {
    if tauri::utils::platform::bundle_type().is_none() {
        return Err("In-place updates require an installed Tyde package. Development builds cannot replace themselves.".into());
    }
    let updates = app.state::<Updates>();
    let _operation = updates
        .operation
        .try_lock()
        .map_err(|_| "An update operation is in progress")?;
    let update = {
        let inner = updates
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inner
            .pending
            .clone()
            .filter(|update| update.version == version)
            .ok_or("This update is no longer available. Check for updates again.")?
    };
    updates.change(&app, |inner| {
        inner.status.phase = UpdatePhase::Downloading;
        inner.status.downloaded = 0;
        inner.status.total = None;
        inner.status.prompt = true;
        inner.status.error = None;
    });
    let result = async {
        let mut last_progress = std::time::Instant::now();
        let mut downloaded = 0;
        let bytes = update
            .download(
                |chunk, total| {
                    downloaded += chunk as u64;
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        updates.change(&app, |inner| {
                            inner.status.downloaded = downloaded;
                            inner.status.total = total;
                        });
                        last_progress = std::time::Instant::now();
                    }
                },
                || {},
            )
            .await
            .map_err(|error| error.to_string())?;
        updates.change(&app, |inner| {
            inner.status.downloaded = bytes.len() as u64;
            inner.status.phase = UpdatePhase::Installing;
        });
        tauri::async_runtime::spawn_blocking(move || update.install(bytes))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        app.request_restart();
        Ok::<_, String>(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(%error, "app update installation failed");
        return Ok(updates.change(&app, |inner| {
            inner.status.phase = UpdatePhase::Error;
            inner.status.error = Some(error);
        }));
    }
    Ok(updates.snapshot())
}

pub fn init(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let preferences_path = app.path().app_config_dir()?.join("updates.json");
    let (preferences, error) = match std::fs::read(&preferences_path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(preferences) => (preferences, None),
            Err(error) => (
                UpdatePreferences {
                    automatic: false,
                    ..Default::default()
                },
                Some(format!("Update preferences could not be read: {error}")),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (UpdatePreferences::default(), None)
        }
        Err(error) => (
            UpdatePreferences {
                automatic: false,
                ..Default::default()
            },
            Some(format!("Update preferences could not be read: {error}")),
        ),
    };
    app.manage(Updates {
        inner: Mutex::new(Inner {
            status: AppUpdateStatus {
                revision: 0,
                current_version: app.package_info().version.to_string(),
                preferences,
                phase: if error.is_some() {
                    UpdatePhase::Error
                } else {
                    UpdatePhase::Idle
                },
                version: None,
                notes: None,
                downloaded: 0,
                total: None,
                prompt: false,
                last_checked: None,
                error,
            },
            pending: None,
            next_check: 0,
            seen_servers: Default::default(),
        }),
        operation: tokio::sync::Mutex::new(()),
        preferences_path,
    });
    app.plugin(tauri_plugin_updater::Builder::new().build())?;
    app.plugin(
        tauri::plugin::Builder::<tauri::Wry, ()>::new("app-updates")
            .invoke_handler(tauri::generate_handler![
                status,
                check_now,
                configure,
                dismiss,
                server_version,
                install
            ])
            .build(),
    )?;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let due = {
                let updates = app.state::<Updates>();
                let inner = updates
                    .inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                inner.status.preferences.automatic && now() >= inner.next_check
            };
            if due && let Err(error) = check(&app, false).await {
                tracing::warn!(%error, "automatic update check failed");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Feed {
        root: String,
        tampered: Arc<AtomicBool>,
        failed: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<()>,
        directory: PathBuf,
    }

    impl Drop for Feed {
        fn drop(&mut self) {
            self.task.abort();
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    impl Feed {
        async fn start() -> Self {
            let (directory, manifest) = assembled_manifest();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let tampered = Arc::new(AtomicBool::new(false));
            let failed = Arc::new(AtomicBool::new(false));
            let server_root = root.clone();
            let tampered_response = tampered.clone();
            let failed_response = failed.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = vec![0; 8192];
                    let length = stream.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..length]);
                    let path = request.split_whitespace().nth(1).unwrap();
                    let code = if failed_response.load(Ordering::Relaxed) {
                        "503 Service Unavailable"
                    } else {
                        "200 OK"
                    };
                    let body = if path.starts_with("/releases?") {
                        let release = |version: &str,
                                       draft: bool,
                                       prerelease: bool,
                                       signed: bool| {
                            json!({
                                "tag_name": version, "draft": draft, "prerelease": prerelease,
                                "assets": if signed { vec![json!({"name": "tyde-update.json"})] } else { vec![] },
                            })
                        };
                        serde_json::to_vec(&vec![
                            release("v2.0.0-beta.2", false, true, true),
                            release("v1.2.0", false, false, true),
                            release("v1.10.0", false, false, true),
                            release("v2.0.0-beta.10", false, false, true),
                            release("v9.0.0", true, false, true),
                            release("v8.0.0", false, false, false),
                            release("../../escape", false, false, true),
                        ])
                        .unwrap()
                    } else if path.ends_with("tyde-update.json") {
                        let version = path.split('/').nth(2).unwrap().trim_start_matches('v');
                        let mut response = manifest.clone();
                        response["version"] = json!(version);
                        for platform in response["platforms"].as_object_mut().unwrap().values_mut()
                        {
                            platform["url"] = json!(format!("{server_root}/package"));
                        }
                        serde_json::to_vec(&response).unwrap()
                    } else if tampered_response.load(Ordering::Relaxed) {
                        b"tampered package".to_vec()
                    } else {
                        include_bytes!("../test-fixtures/updater/package").to_vec()
                    };
                    let header = format!(
                        "HTTP/1.1 {code}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(&body).await.unwrap();
                }
            });
            Self {
                root,
                tampered,
                failed,
                task,
                directory,
            }
        }
    }

    fn assembled_manifest() -> (PathBuf, serde_json::Value) {
        let directory =
            std::env::temp_dir().join(format!("tyde-update-feed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let targets = [
            ("aarch64-apple-darwin", "darwin-aarch64", vec!["app"]),
            ("x86_64-apple-darwin", "darwin-x86_64", vec!["app"]),
            (
                "aarch64-unknown-linux-gnu",
                "linux-aarch64",
                vec!["appimage", "deb", "rpm"],
            ),
            (
                "x86_64-unknown-linux-gnu",
                "linux-x86_64",
                vec!["appimage", "deb", "rpm"],
            ),
            (
                "x86_64-pc-windows-msvc",
                "windows-x86_64",
                vec!["nsis", "msi"],
            ),
        ];
        for (target, base, installers) in targets {
            let mut platforms = serde_json::Map::new();
            for installer in installers {
                let extension = match installer {
                    "app" => "app.tar.gz",
                    "appimage" => "AppImage",
                    "nsis" => "exe",
                    other => other,
                };
                let entry = json!({
                    "url": format!("https://github.com/tigy32/Tyde/releases/download/v1.10.0/tyde-update-1.10.0-{target}.{extension}"),
                    "signature": include_str!("../test-fixtures/updater/package.sig").trim(),
                });
                if matches!(installer, "app" | "appimage" | "nsis") {
                    platforms.insert(base.to_owned(), entry.clone());
                }
                platforms.insert(format!("{base}-{installer}"), entry);
            }
            let fragment = json!({"version": "1.10.0", "platforms": platforms});
            std::fs::write(
                directory.join(format!("update-{target}.json")),
                serde_json::to_vec(&fragment).unwrap(),
            )
            .unwrap();
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let output = directory.join("tyde-update.json");
        let assemble = || {
            std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
                .arg(root.join("tools/update_manifest.py"))
                .args(["assemble", "--tag", "v1.10.0", "--fragments"])
                .arg(&directory)
                .arg("--output")
                .arg(&output)
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .output()
                .unwrap()
        };
        let result = assemble();
        assert!(
            result.status.success(),
            "Manifest assembly failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        validate_published_assets(root, &directory, &manifest);
        std::fs::remove_file(directory.join("update-aarch64-apple-darwin.json")).unwrap();
        assert!(
            !assemble().status.success(),
            "An incomplete platform set must block release publication"
        );
        (directory, manifest)
    }

    fn validate_published_assets(
        root: &std::path::Path,
        directory: &std::path::Path,
        manifest: &serde_json::Value,
    ) {
        let mut names: std::collections::BTreeSet<String> = [
            "tyde-update.json",
            "tyde-server-aarch64-apple-darwin.zip",
            "tyde-server-x86_64-apple-darwin.zip",
            "tyde-server-aarch64-unknown-linux-musl.zip",
            "tyde-server-x86_64-unknown-linux-musl.zip",
            "tyde-server-x86_64-pc-windows-msvc.zip",
            "Tyde_1.10.0_aarch64-apple-darwin.dmg",
            "Tyde_1.10.0_x86_64-apple-darwin.dmg",
            "Tyde_1.10.0_amd64.AppImage",
            "Tyde_1.10.0_aarch64.AppImage",
            "Tyde_1.10.0_amd64.deb",
            "Tyde_1.10.0_arm64.deb",
            "Tyde-1.10.0-1.x86_64.rpm",
            "Tyde-1.10.0-1.aarch64.rpm",
            "Tyde_1.10.0_x64-setup.exe",
            "Tyde_1.10.0_x64_en-US.msi",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for name in names.clone() {
            if name.ends_with(".AppImage") || name.ends_with(".deb") {
                names.insert(format!("{name}.sha256"));
            }
            if [".AppImage", ".deb", ".rpm", ".exe", ".msi"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                names.insert(format!("{name}.sig"));
            }
        }
        for platform in manifest["platforms"].as_object().unwrap().values() {
            let name = platform["url"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap();
            names.insert(name.to_owned());
            names.insert(format!("{name}.sig"));
        }
        let input = directory.join("release.json");
        let validate = |names: &std::collections::BTreeSet<String>| {
            let release = json!({"tagName": "v1.10.0", "isDraft": false, "isPrerelease": false,
                "assets": names.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()});
            std::fs::write(&input, serde_json::to_vec(&release).unwrap()).unwrap();
            std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
                .arg(root.join("tools/release_tool.py"))
                .args([
                    "validate-release",
                    "v1.10.0",
                    "--require-published",
                    "--input",
                ])
                .arg(&input)
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .output()
                .unwrap()
        };
        let complete = validate(&names);
        assert!(
            complete.status.success(),
            "Complete signed release was rejected: {}",
            String::from_utf8_lossy(&complete.stderr)
        );
        names.remove("tyde-update.json");
        assert!(
            !validate(&names).status.success(),
            "Publishing without the updater manifest must fail"
        );
        names.insert("tyde-update.json".into());
        names.insert("unrecognized.sig".into());
        assert!(
            !validate(&names).status.success(),
            "Only signatures of recognized packages may be added by Tauri Action"
        );
    }

    #[tokio::test]
    async fn published_channels_download_only_verified_updates_over_http() {
        let feed = Feed::start().await;
        let current = Version::parse("1.0.0").unwrap();
        let releases = format!("{}/releases", feed.root);
        let downloads = format!("{}/downloads", feed.root);
        let (stable, endpoint) =
            release_endpoint(UpdateChannel::Release, &current, &releases, &downloads)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stable.to_string(), "1.10.0");
        let (preview, _) =
            release_endpoint(UpdateChannel::Preview, &current, &releases, &downloads)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(preview.to_string(), "2.0.0-beta.10");
        assert!(
            release_endpoint(UpdateChannel::Release, &preview, &releases, &downloads)
                .await
                .unwrap()
                .is_none(),
            "Changing channels must never downgrade the app"
        );

        let mut context = tauri::test::mock_context(tauri::test::noop_assets());
        context.config_mut().plugins.0.insert(
            "updater".into(),
            json!({
                "pubkey": include_str!("../test-fixtures/updater/public.key").trim(),
                "dangerousInsecureTransportProtocol": true,
            }),
        );
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        let updater = app
            .updater_builder()
            .target("linux-x86_64")
            .endpoints(vec![endpoint])
            .unwrap()
            .build()
            .unwrap();
        let update = updater.check().await.unwrap().unwrap();
        assert_eq!(update.version, "1.10.0");
        let bytes = update.download(|_, _| {}, || {}).await.unwrap();
        assert_eq!(bytes, include_bytes!("../test-fixtures/updater/package"));
        feed.tampered.store(true, Ordering::Relaxed);
        assert!(
            update.download(|_, _| {}, || {}).await.is_err(),
            "Tampered bytes must never reach the installer"
        );
        feed.failed.store(true, Ordering::Relaxed);
        assert!(
            release_endpoint(UpdateChannel::Release, &current, &releases, &downloads)
                .await
                .is_err(),
            "A failed check must not report that the app is up to date"
        );
    }
}
