use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
pub(crate) fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("store path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create store directory {}: {err}",
            parent.display()
        )
    })?;

    let file_name = path
        .file_name()
        .ok_or_else(|| format!("store path has no file name: {}", path.display()))?
        .to_string_lossy();
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp"));

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(|err| {
            format!(
                "failed to create temp store file {}: {err}",
                tmp_path.display()
            )
        })?;
    file.write_all(bytes).map_err(|err| {
        format!(
            "failed to write temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    file.sync_all().map_err(|err| {
        format!(
            "failed to sync temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    drop(file);

    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
        format!(
            "failed to set owner-only permissions on temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    std::fs::rename(&tmp_path, path).map_err(|err| {
        format!(
            "failed to atomically replace store file {}: {err}",
            path.display()
        )
    })?;
    enforce_owner_only_file(path)
}

#[cfg(windows)]
pub(crate) fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("store path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create store directory {}: {error}",
            parent.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("store path has no file name: {}", path.display()))?
        .to_string_lossy();
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = windows::create_owner_only_file(&tmp_path)?;
    file.write_all(bytes).map_err(|error| {
        format!(
            "failed to write temp store file {}: {error}",
            tmp_path.display()
        )
    })?;
    file.sync_all().map_err(|error| {
        format!(
            "failed to sync temp store file {}: {error}",
            tmp_path.display()
        )
    })?;
    drop(file);
    std::fs::rename(&tmp_path, path).map_err(|error| {
        format!(
            "failed to atomically replace store file {}: {error}",
            path.display()
        )
    })?;
    enforce_owner_only_file(path)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn atomic_write_owner_only(_path: &Path, _bytes: &[u8]) -> Result<(), String> {
    Err("owner-only file permissions are unsupported on this platform".to_owned())
}

#[cfg(unix)]
pub(crate) fn enforce_owner_only_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| format!("failed to stat store file {}: {err}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("store path {} is not a file", path.display()));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            format!(
                "failed to enforce owner-only permissions on store file {}: {err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn enforce_owner_only_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("failed to stat store file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("store path {} is not a file", path.display()));
    }
    windows::enforce_owner_only_file(path)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn enforce_owner_only_file(_path: &Path) -> Result<(), String> {
    Err("owner-only file permissions are unsupported on this platform".to_owned())
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::{
        Foundation::{
            ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
            GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: this allocation is returned by the Windows security APIs.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn last_error(context: &str) -> String {
        format!("{context}: {}", std::io::Error::last_os_error())
    }

    fn private_descriptor() -> Result<LocalAllocation, String> {
        // SAFETY: handles and aligned token storage outlive every API call;
        // converted SID/descriptor allocations are owned and freed exactly once.
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(last_error("failed to query store owner token"));
            }
            let token = OwnedHandle::from_raw_handle(token);
            let mut length = 0;
            if GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length)
                != 0
                || GetLastError() != ERROR_INSUFFICIENT_BUFFER
            {
                return Err(last_error("failed to size store owner token"));
            }
            let mut storage = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
            if GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                storage.as_mut_ptr().cast(),
                length,
                &mut length,
            ) == 0
            {
                return Err(last_error("failed to read store owner token"));
            }
            let user = &*storage.as_ptr().cast::<TOKEN_USER>();
            let mut sid = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(last_error("failed to encode store owner SID"));
            }
            let sid_allocation = LocalAllocation(sid.cast());
            let mut sid_length = 0;
            while *sid.add(sid_length) != 0 {
                sid_length += 1;
            }
            let sid = String::from_utf16(std::slice::from_raw_parts(sid, sid_length))
                .map_err(|error| format!("failed to decode store owner SID: {error}"))?;
            drop(sid_allocation);
            // A protected DACL prevents directory inheritance from granting
            // another account access, including during temporary-file creation.
            let sddl: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})")
                .encode_utf16()
                .chain([0])
                .collect();
            let mut descriptor = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(last_error(
                    "failed to create owner-only security descriptor",
                ));
            }
            Ok(LocalAllocation(descriptor))
        }
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, String> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err("store path contains a NUL character".to_owned());
        }
        wide.push(0);
        Ok(wide)
    }

    pub(super) fn create_owner_only_file(path: &Path) -> Result<File, String> {
        let descriptor = private_descriptor()?;
        let path_wide = wide_path(path)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: paths and the security descriptor remain valid during creation;
        // a successful handle is transferred to File for exclusive ownership.
        unsafe {
            let handle = CreateFileW(
                path_wide.as_ptr(),
                FILE_GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(last_error(&format!(
                    "failed to create private store file {}",
                    path.display()
                )));
            }
            Ok(File::from_raw_handle(handle))
        }
    }

    pub(super) fn enforce_owner_only_file(path: &Path) -> Result<(), String> {
        let descriptor = private_descriptor()?;
        let path_wide = wide_path(path)?;
        // SAFETY: the owner and DACL pointers are borrowed from the live,
        // validated security descriptor for the duration of SetNamedSecurityInfoW.
        unsafe {
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl = null_mut();
            let mut owner = null_mut();
            if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) == 0
                || present == 0
                || dacl.is_null()
                || GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) == 0
            {
                return Err(last_error("failed to read owner-only security descriptor"));
            }
            let result = SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner,
                null_mut(),
                dacl,
                null(),
            );
            if result != ERROR_SUCCESS {
                return Err(format!(
                    "failed to enforce owner-only permissions on store file {}: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(result as i32)
                ));
            }
        }
        Ok(())
    }
}
