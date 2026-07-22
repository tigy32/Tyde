//! Tycode settings profiles.
//!
//! A Tycode profile is a settings file: the shared `~/.tycode/settings.toml`
//! is the `default` profile and every `~/.tycode/profiles/<name>.toml` file
//! is a named profile. Tyde never copies or projects these files — sessions
//! launch the pinned Tycode subprocess directly against the resolved profile
//! file (`--settings-path`), and the settings editor persists edits through
//! the subprocess against that same file. Every function takes the Tycode
//! home directory explicitly; [`crate::backend::tycode`] owns home
//! resolution (including its test override).

use std::fs;
use std::path::{Path, PathBuf};

pub(crate) use protocol::tycode_config::TYCODE_DEFAULT_PROFILE;

pub(crate) const TYCODE_PROFILES_DIR: &str = "profiles";
pub(crate) const TYCODE_SETTINGS_FILE: &str = "settings.toml";
const TYCODE_PROFILE_EXTENSION: &str = "toml";

/// File names of the retired Tyde-managed settings projection. Exact matches
/// are removed by [`cleanup_legacy_projection_artifacts_in`]; temp/journal
/// artifacts are matched by their reserved `.tyde-settings.` prefix.
const LEGACY_PROJECTION_FILES: &[&str] = &[
    "tyde-settings.toml",
    "tyde-settings.provenance.json",
    "tyde-settings.transaction.json",
    "tyde-settings.recovery.json",
    "tyde-settings.lock",
];
const LEGACY_PROJECTION_TEMP_PREFIX: &str = ".tyde-settings.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TycodeProfileRef {
    /// `"default"` for the shared settings file, else the profile file stem.
    pub(crate) name: String,
    /// The settings file Tycode is launched against for this profile.
    pub(crate) settings_path: PathBuf,
}

fn default_profile(home: &Path) -> TycodeProfileRef {
    TycodeProfileRef {
        name: TYCODE_DEFAULT_PROFILE.to_owned(),
        settings_path: home.join(TYCODE_SETTINGS_FILE),
    }
}

fn named_profile_path(home: &Path, name: &str) -> PathBuf {
    home.join(TYCODE_PROFILES_DIR)
        .join(format!("{name}.{TYCODE_PROFILE_EXTENSION}"))
}

/// Tyde's profile-name grammar: `^[a-z0-9][a-z0-9_-]{0,63}$` (the same
/// grammar Hermes profiles use). Tycode has no native profile concept, so
/// this is Tyde's own convention; enforcing it structurally rules out path
/// traversal in `profiles/<name>.toml`.
pub(crate) fn is_valid_profile_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if name.len() > 64 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Enumerate profiles under one Tycode home: the default profile (the shared
/// settings file) first, then named `profiles/<name>.toml` files sorted by
/// name. A missing `profiles/` directory means no named profiles; an
/// unreadable one is a visible error.
pub(crate) fn discover_profiles_in(home: &Path) -> Result<Vec<TycodeProfileRef>, String> {
    let mut profiles = vec![default_profile(home)];
    let profiles_dir = home.join(TYCODE_PROFILES_DIR);
    if !profiles_dir.is_dir() {
        return Ok(profiles);
    }
    let entries = fs::read_dir(&profiles_dir).map_err(|error| {
        format!(
            "Failed to list Tycode profiles in {}: {error}",
            profiles_dir.display()
        )
    })?;
    let mut named = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read a Tycode profile entry in {}: {error}",
                profiles_dir.display()
            )
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some(TYCODE_PROFILE_EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // Anything that does not follow Tyde's profile grammar is not a
        // profile (and `default` is reserved for the shared settings file).
        if !is_valid_profile_name(name) || name == TYCODE_DEFAULT_PROFILE {
            continue;
        }
        named.push(TycodeProfileRef {
            name: name.to_owned(),
            settings_path: path,
        });
    }
    named.sort_by(|a, b| a.name.cmp(&b.name));
    profiles.extend(named);
    Ok(profiles)
}

/// Resolve a session-setting profile name to a profile ref. `None`, empty and
/// `"default"` all mean the shared settings file. Named profiles must exist
/// on disk; an unknown or invalid name is a visible error, never a silent
/// fallback to the default profile.
pub(crate) fn resolve_profile_ref_in(
    home: &Path,
    name: Option<&str>,
) -> Result<TycodeProfileRef, String> {
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    let Some(name) = name.filter(|name| *name != TYCODE_DEFAULT_PROFILE) else {
        return Ok(default_profile(home));
    };
    if !is_valid_profile_name(name) {
        return Err(format!("invalid Tycode profile name '{name}'"));
    }
    let path = named_profile_path(home, name);
    if !path.is_file() {
        return Err(format!(
            "Tycode profile '{name}' does not exist at {}",
            path.display()
        ));
    }
    Ok(TycodeProfileRef {
        name: name.to_owned(),
        settings_path: path,
    })
}

/// Create `profiles/<name>.toml` as a byte-for-byte copy of the `copy_from`
/// profile's settings file (default: the shared settings file). Creation
/// refuses to overwrite an existing profile and requires the source file to
/// exist — copying nothing would silently invent settings.
pub(crate) fn create_profile_in(
    home: &Path,
    name: &str,
    copy_from: Option<&str>,
) -> Result<TycodeProfileRef, String> {
    if !is_valid_profile_name(name) || name == TYCODE_DEFAULT_PROFILE {
        return Err(format!("invalid Tycode profile name '{name}'"));
    }
    let source = resolve_profile_ref_in(home, copy_from)?;
    if !source.settings_path.is_file() {
        return Err(format!(
            "Cannot create Tycode profile '{name}': source settings file {} does not exist; \
             run Tycode once to create it",
            source.settings_path.display()
        ));
    }
    let destination = named_profile_path(home, name);
    if destination.exists() {
        return Err(format!("Tycode profile '{name}' already exists"));
    }
    let profiles_dir = home.join(TYCODE_PROFILES_DIR);
    fs::create_dir_all(&profiles_dir).map_err(|error| {
        format!(
            "Failed to create Tycode profiles directory {}: {error}",
            profiles_dir.display()
        )
    })?;
    let bytes = fs::read(&source.settings_path).map_err(|error| {
        format!(
            "Failed to read Tycode settings {}: {error}",
            source.settings_path.display()
        )
    })?;
    write_atomic(&destination, &bytes)?;
    Ok(TycodeProfileRef {
        name: name.to_owned(),
        settings_path: destination,
    })
}

/// Delete `profiles/<name>.toml`. The default profile (the shared settings
/// file) cannot be deleted, and deleting an unknown profile is a visible
/// error.
pub(crate) fn delete_profile_in(home: &Path, name: &str) -> Result<(), String> {
    if name == TYCODE_DEFAULT_PROFILE {
        return Err("The default Tycode profile cannot be deleted".to_owned());
    }
    if !is_valid_profile_name(name) {
        return Err(format!("invalid Tycode profile name '{name}'"));
    }
    let path = named_profile_path(home, name);
    if !path.is_file() {
        return Err(format!(
            "Tycode profile '{name}' does not exist at {}",
            path.display()
        ));
    }
    fs::remove_file(&path).map_err(|error| {
        format!(
            "Failed to delete Tycode profile {}: {error}",
            path.display()
        )
    })
}

/// Atomic write via a same-directory tempfile so a crash cannot leave a
/// half-written profile. Profile files may hold API keys, so they are
/// created owner-only like Tycode's own settings file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tyde-profile-")
        .tempfile_in(directory)
        .map_err(|error| {
            format!(
                "Failed to create a tempfile in {}: {error}",
                directory.display()
            )
        })?;
    use std::io::Write as _;
    temp.write_all(bytes)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to restrict {}: {error}", path.display()))?;
    }
    temp.persist(path)
        .map_err(|error| format!("Failed to persist {}: {error}", path.display()))?;
    Ok(())
}

/// Remove every file left behind by the retired Tyde-managed settings
/// projection. Returns the removed paths; a file that cannot be removed is a
/// visible error so stale copies never linger silently.
pub(crate) fn cleanup_legacy_projection_artifacts_in(home: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    if !home.is_dir() {
        return Ok(removed);
    }
    let entries = fs::read_dir(home)
        .map_err(|error| format!("Failed to list {}: {error}", home.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("Failed to read an entry in {}: {error}", home.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let legacy = LEGACY_PROJECTION_FILES.contains(&name)
            || name.starts_with(LEGACY_PROJECTION_TEMP_PREFIX);
        if !legacy {
            continue;
        }
        fs::remove_file(&path).map_err(|error| {
            format!(
                "Failed to remove retired Tycode projection artifact {}: {error}",
                path.display()
            )
        })?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp Tycode home")
    }

    #[test]
    fn profile_name_grammar_matches_hermes_grammar() {
        for valid in ["a", "0", "work", "low-cost_2", &"a".repeat(64)] {
            assert!(is_valid_profile_name(valid), "{valid} should be valid");
        }
        for invalid in [
            "",
            "-lead",
            "_lead",
            "UPPER",
            "sp ace",
            "a/b",
            "a.b",
            &"a".repeat(65),
        ] {
            assert!(
                !is_valid_profile_name(invalid),
                "{invalid} should be invalid"
            );
        }
    }

    #[test]
    fn discovery_lists_default_first_then_sorted_named_profiles() {
        let home = temp_home();
        std::fs::write(home.path().join(TYCODE_SETTINGS_FILE), b"a = 1\n").unwrap();
        let profiles_dir = home.path().join(TYCODE_PROFILES_DIR);
        std::fs::create_dir(&profiles_dir).unwrap();
        std::fs::write(profiles_dir.join("zeta.toml"), b"z = 1\n").unwrap();
        std::fs::write(profiles_dir.join("alpha.toml"), b"a = 1\n").unwrap();
        // Non-profiles: wrong extension, invalid grammar, reserved name.
        std::fs::write(profiles_dir.join("notes.txt"), b"x\n").unwrap();
        std::fs::write(profiles_dir.join("BAD.toml"), b"x\n").unwrap();
        std::fs::write(profiles_dir.join("default.toml"), b"x\n").unwrap();

        let profiles = discover_profiles_in(home.path()).expect("discover profiles");
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["default", "alpha", "zeta"]);
        assert_eq!(
            profiles[0].settings_path,
            home.path().join(TYCODE_SETTINGS_FILE)
        );
        assert_eq!(profiles[2].settings_path, profiles_dir.join("zeta.toml"));
    }

    #[test]
    fn resolution_defaults_and_refuses_unknown_or_invalid_names() {
        let home = temp_home();
        for default_ref in [None, Some(""), Some("  "), Some("default")] {
            let profile = resolve_profile_ref_in(home.path(), default_ref).expect("default");
            assert_eq!(profile.name, "default");
            assert_eq!(
                profile.settings_path,
                home.path().join(TYCODE_SETTINGS_FILE)
            );
        }
        // The default profile resolves even when settings.toml does not exist
        // yet — Tycode creates its own defaults on first launch.
        assert!(!home.path().join(TYCODE_SETTINGS_FILE).exists());

        let missing = resolve_profile_ref_in(home.path(), Some("work")).unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
        let invalid = resolve_profile_ref_in(home.path(), Some("../escape")).unwrap_err();
        assert!(invalid.contains("invalid Tycode profile name"), "{invalid}");
    }

    #[test]
    fn create_copies_source_bytes_and_refuses_overwrite() {
        let home = temp_home();
        std::fs::write(home.path().join(TYCODE_SETTINGS_FILE), b"key = \"v\"\n").unwrap();

        let created = create_profile_in(home.path(), "work", None).expect("create profile");
        assert_eq!(created.name, "work");
        assert_eq!(
            std::fs::read(&created.settings_path).unwrap(),
            b"key = \"v\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&created.settings_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let duplicate = create_profile_in(home.path(), "work", None).unwrap_err();
        assert!(duplicate.contains("already exists"), "{duplicate}");

        // Copying from a named profile copies that profile's bytes.
        std::fs::write(created.settings_path, b"key = \"work\"\n").unwrap();
        let clone = create_profile_in(home.path(), "work2", Some("work")).expect("clone");
        assert_eq!(
            std::fs::read(&clone.settings_path).unwrap(),
            b"key = \"work\"\n"
        );
    }

    #[test]
    fn create_without_source_settings_is_a_visible_error() {
        let home = temp_home();
        let error = create_profile_in(home.path(), "work", None).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn delete_removes_named_profiles_and_protects_the_default() {
        let home = temp_home();
        std::fs::write(home.path().join(TYCODE_SETTINGS_FILE), b"a = 1\n").unwrap();
        let created = create_profile_in(home.path(), "work", None).expect("create profile");

        delete_profile_in(home.path(), "work").expect("delete profile");
        assert!(!created.settings_path.exists());

        let missing = delete_profile_in(home.path(), "work").unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
        let protected = delete_profile_in(home.path(), "default").unwrap_err();
        assert!(protected.contains("cannot be deleted"), "{protected}");
        assert!(home.path().join(TYCODE_SETTINGS_FILE).exists());
    }

    #[test]
    fn cleanup_removes_only_retired_projection_artifacts() {
        let home = temp_home();
        std::fs::write(home.path().join(TYCODE_SETTINGS_FILE), b"a = 1\n").unwrap();
        for name in [
            "tyde-settings.toml",
            "tyde-settings.provenance.json",
            "tyde-settings.transaction.json",
            "tyde-settings.recovery.json",
            "tyde-settings.lock",
            ".tyde-settings.prejournal-save-managed-x.txn",
        ] {
            std::fs::write(home.path().join(name), b"stale").unwrap();
        }

        let removed =
            cleanup_legacy_projection_artifacts_in(home.path()).expect("cleanup artifacts");
        assert_eq!(removed.len(), 6);
        assert!(home.path().join(TYCODE_SETTINGS_FILE).exists());
        assert!(!home.path().join("tyde-settings.toml").exists());
        assert!(
            cleanup_legacy_projection_artifacts_in(home.path())
                .expect("idempotent cleanup")
                .is_empty()
        );
    }
}
