use serde::{Deserialize, Serialize};

const RELEASES_API_URL: &str = "https://api.github.com/repos/tigy32/Tyde/releases?per_page=20";
const RELEASE_URL_PREFIX: &str = "https://github.com/tigy32/Tyde/releases/tag/";
const DOWNLOAD_URL_PREFIX: &str = "https://github.com/tigy32/Tyde/releases/download/";

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    current_version: String,
    available: Option<AvailableUpdate>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableUpdate {
    version: String,
    release_url: String,
    asset_name: Option<String>,
    download_url: Option<String>,
}

pub async fn check(app: tauri::AppHandle) -> Result<UpdateCheckResult, String> {
    let current = app.package_info().version.clone();
    let include_prereleases = !current.pre.is_empty();
    let releases = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("Tyde/{} update-check", current))
        .build()
        .map_err(|error| format!("Failed to prepare the update check: {error}"))?
        .get(RELEASES_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|error| format!("Could not reach GitHub Releases: {error}"))?
        .error_for_status()
        .map_err(|error| format!("GitHub Releases rejected the update check: {error}"))?
        .json::<Vec<GithubRelease>>()
        .await
        .map_err(|error| format!("GitHub Releases returned an invalid response: {error}"))?;

    let newest = releases
        .into_iter()
        .filter(|release| !release.draft && (include_prereleases || !release.prerelease))
        .filter_map(|release| {
            let version: semver::Version = release.tag_name.strip_prefix('v')?.parse().ok()?;
            (version > current).then_some((version, release))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right));

    let available = newest
        .map(|(version, release)| available_update(version.to_string(), release))
        .transpose()?;

    Ok(UpdateCheckResult {
        current_version: current.to_string(),
        available,
    })
}

fn available_update(version: String, release: GithubRelease) -> Result<AvailableUpdate, String> {
    if !release.html_url.starts_with(RELEASE_URL_PREFIX) {
        return Err("GitHub returned an untrusted release URL".to_owned());
    }

    let asset = preferred_asset(&release.assets);
    if let Some(asset) = asset
        && !asset.browser_download_url.starts_with(DOWNLOAD_URL_PREFIX)
    {
        return Err("GitHub returned an untrusted download URL".to_owned());
    }

    Ok(AvailableUpdate {
        version,
        release_url: release.html_url,
        asset_name: asset.map(|asset| asset.name.clone()),
        download_url: asset.map(|asset| asset.browser_download_url.clone()),
    })
}

fn preferred_asset(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    let suffixes = preferred_asset_suffixes();
    suffixes
        .iter()
        .find_map(|suffix| assets.iter().find(|asset| asset.name.ends_with(suffix)))
}

#[cfg(target_os = "macos")]
fn preferred_asset_suffixes() -> Vec<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => vec!["_aarch64-apple-darwin.dmg"],
        "x86_64" => vec!["_x86_64-apple-darwin.dmg"],
        _ => Vec::new(),
    }
}

#[cfg(target_os = "windows")]
fn preferred_asset_suffixes() -> Vec<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => vec!["_x64-setup.exe", "_x64_en-US.msi"],
        _ => Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn preferred_asset_suffixes() -> Vec<&'static str> {
    let debian = std::fs::read_to_string("/etc/os-release").is_ok_and(|contents| {
        contents.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            (lower.starts_with("id=") || lower.starts_with("id_like="))
                && (lower.contains("debian") || lower.contains("ubuntu"))
        })
    });
    let rpm = std::fs::read_to_string("/etc/os-release").is_ok_and(|contents| {
        contents.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            (lower.starts_with("id=") || lower.starts_with("id_like="))
                && (lower.contains("fedora") || lower.contains("rhel") || lower.contains("suse"))
        })
    });

    match (std::env::consts::ARCH, debian, rpm) {
        ("aarch64", true, _) => vec!["_arm64.deb", "_aarch64.AppImage", "-1.aarch64.rpm"],
        ("aarch64", _, true) => vec!["-1.aarch64.rpm", "_aarch64.AppImage", "_arm64.deb"],
        ("aarch64", _, _) => vec!["_aarch64.AppImage", "_arm64.deb", "-1.aarch64.rpm"],
        ("x86_64", true, _) => vec!["_amd64.deb", "_amd64.AppImage", "-1.x86_64.rpm"],
        ("x86_64", _, true) => vec!["-1.x86_64.rpm", "_amd64.AppImage", "_amd64.deb"],
        ("x86_64", _, _) => vec!["_amd64.AppImage", "_amd64.deb", "-1.x86_64.rpm"],
        _ => Vec::new(),
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn preferred_asset_suffixes() -> Vec<&'static str> {
    Vec::new()
}
