pub mod cli;
pub mod consts;
pub mod data;
pub mod envs;
pub mod errors;
pub mod home;
pub mod input;
pub mod kdl;
pub mod pane_size;
pub mod plugin_api;
pub mod position;
pub mod session_serialization;
pub mod setup;
pub mod shared;

#[cfg(windows)]
pub mod windows_utils;

// The following modules can't be used when targeting wasm
#[cfg(not(target_family = "wasm"))]
pub mod channels; // Requires async_std
#[cfg(not(target_family = "wasm"))]
pub mod common_path;
#[cfg(not(target_family = "wasm"))]
pub mod downloader; // Requires async_std
#[cfg(not(target_family = "wasm"))]
pub mod ipc; // Requires interprocess
#[cfg(not(target_family = "wasm"))]
pub mod logging; // Requires log4rs
#[cfg(not(target_family = "wasm"))]
pub mod sessions;
#[cfg(all(not(target_family = "wasm"), feature = "web_server_capability"))]
pub mod web_authentication_tokens;
#[cfg(all(not(target_family = "wasm"), feature = "web_server_capability"))]
#[cfg(unix)]
pub mod web_server_commands;

// TODO(hartan): Remove this re-export for the next minor release.
pub use ::prost;

#[cfg(windows)]
pub fn is_socket(file: &std::fs::DirEntry) -> std::io::Result<bool> {
    use std::ffi::{OsStr, OsString};
    fn convert_path(pipe_name: &OsStr, hostname: Option<&OsStr>) -> Vec<u16> {
        static PREFIX_LITERAL: &str = r"\\";
        static PIPEFS_LITERAL: &str = r"\pipe\";

        let hostname = hostname.unwrap_or_else(|| OsStr::new("."));

        let mut path = OsString::with_capacity(
            PREFIX_LITERAL.len() + hostname.len() + PIPEFS_LITERAL.len() + pipe_name.len(),
        );
        path.push(PREFIX_LITERAL);
        path.push(hostname);
        path.push(PIPEFS_LITERAL);
        path.push(pipe_name);

        let mut path = path.encode_wide().collect::<Vec<u16>>();
        path.push(0); // encode_wide does not include the terminating NULL, so we have to add it ourselves
        path
    }

    use std::{os::windows::ffi::OsStrExt, ptr};
    use winapi::um::{
        fileapi::{CreateFileW, GetFileType, OPEN_EXISTING},
        handleapi::INVALID_HANDLE_VALUE,
        winbase::FILE_TYPE_PIPE,
        winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ},
    };

    let path = convert_path(file.path().as_os_str(), None);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let file_type = unsafe { GetFileType(handle) };
    if file_type == 0 {
        let error = std::io::Error::last_os_error();
        return Err(error);
    }

    Ok(file_type == FILE_TYPE_PIPE)
}

#[cfg(unix)]
pub fn is_socket(file: &std::fs::DirEntry) -> std::io::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    Ok(file.file_type()?.is_socket())
}
