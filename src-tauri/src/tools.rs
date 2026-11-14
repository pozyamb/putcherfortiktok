//! Locating and launching the ffmpeg / ffprobe sidecars.

use std::path::PathBuf;
use std::process::Command;

/// Rust target triple the app was built for; sidecars in `src-tauri/binaries`
/// carry it as a suffix (Tauri `externalBin` convention).
const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// Finds a tool by name, in order:
/// 1. next to the running executable (bundled app, or dev build where Tauri copied it);
/// 2. `src-tauri/binaries/<name>-<triple>.exe` (dev, after `scripts/fetch-ffmpeg.ps1`);
/// 3. the system PATH.
pub fn locate(name: &str) -> Option<PathBuf> {
    let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(&exe_name);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!("{name}-{TARGET_TRIPLE}{}", std::env::consts::EXE_SUFFIX));
    if dev.is_file() {
        return Some(dev);
    }

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(&exe_name))
            .find(|p| p.is_file())
    })
}

/// A `Command` that never flashes a console window on Windows.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn ffmpeg() -> Result<PathBuf, String> {
    locate("ffmpeg").ok_or_else(|| {
        "ffmpeg не найден. Запустите scripts/fetch-ffmpeg.ps1, чтобы скачать его.".to_string()
    })
}

pub fn ffprobe() -> Result<PathBuf, String> {
    locate("ffprobe").ok_or_else(|| {
        "ffprobe не найден. Запустите scripts/fetch-ffmpeg.ps1, чтобы скачать его.".to_string()
    })
}
