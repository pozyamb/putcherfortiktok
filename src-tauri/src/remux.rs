//! Remux mode: stream copy into a TikTok-friendly MP4 (no re-encoding).
//!
//! - first video + first audio stream only, everything else dropped
//! - global metadata and chapters stripped
//! - BT.709 colour tags written into the bitstream when the source has none
//! - `moov` atom moved to the front (`+faststart`)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::boxpatch::{self, RatePatch};
use crate::probe::ProbeReport;

/// What to do besides the plain remux.
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Options {
    /// EXPERIMENTAL: stretch video timestamps N× (`-itsscale N`), so the file declares
    /// 1/N of the real frame rate. 0 or 1 = off.
    pub retime_factor: u32,
    /// EXPERIMENTAL: playback-rate hint written into the header after a retime.
    pub rate_patch: RatePatch,
    /// Overwrite the last audio frames the way the reference web optimizer does.
    pub audio_tail: bool,
}

impl Options {
    pub fn factor(&self) -> u32 {
        self.retime_factor.clamp(1, 16)
    }
}

/// Patches applied to the finished file, after ffmpeg has closed it.
pub fn post_process(output: &Path, opts: Options) -> Result<(), String> {
    if opts.factor() > 1 {
        boxpatch::patch_file(output, opts.factor(), opts.rate_patch)?;
    }
    if opts.audio_tail {
        boxpatch::patch_audio_tail(output, boxpatch::AUDIO_TAIL_FRAMES)?;
    }
    Ok(())
}
use crate::tools;

/// Running ffmpeg processes by job id, so they can be cancelled.
#[derive(Default, Clone)]
pub struct Jobs(Arc<Mutex<HashMap<u64, Child>>>);

impl Jobs {
    pub fn cancel(&self, id: u64) -> bool {
        let mut map = self.0.lock().unwrap();
        match map.get_mut(&id) {
            Some(child) => child.kill().is_ok(),
            None => false,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job_id: u64,
    pub percent: f64,
    pub speed: Option<f64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Done {
    pub job_id: u64,
    pub output: String,
    pub size_bytes: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failed {
    pub job_id: u64,
    pub message: String,
    pub cancelled: bool,
}

/// `C:\v\clip.mov` → `C:\v\clip_tiktok.mp4`, or `clip_tiktok (2).mp4` if taken.
pub fn output_path(input: &Path) -> PathBuf {
    let dir = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let mut candidate = dir.join(format!("{stem}_tiktok.mp4"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}_tiktok ({n}).mp4"));
        n += 1;
    }
    candidate
}

/// ffmpeg arguments for a remux of `report`'s file into `output`.
pub fn build_args(report: &ProbeReport, output: &Path, opts: Options) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-v", "error"].map(String::from).to_vec();
    let factor = opts.factor();
    if factor > 1 {
        // The same file twice: input 0 with stretched timestamps gives the video,
        // input 1 with untouched timestamps gives the audio.
        a.extend(["-itsscale".to_string(), factor.to_string(), "-i".to_string(), report.path.clone()]);
        a.extend(["-i".to_string(), report.path.clone()]);
        a.extend(["-map", "0:v:0", "-map", "1:a:0?", "-c", "copy"].map(String::from));
    } else {
        a.extend(["-i".to_string(), report.path.clone()]);
        a.extend(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy"].map(String::from));
    }

    // Tag BT.709 in the bitstream itself; container-level flags are ignored with stream copy.
    // Skipped with the audio tail patch: the reference file had an untouched video bitstream.
    if let (Some(v), false) = (&report.video, opts.audio_tail) {
        if v.color_primaries.is_none() && v.bit_depth == 8 {
            let bsf = match v.codec.as_str() {
                "h264" => Some("h264_metadata"),
                "hevc" => Some("hevc_metadata"),
                _ => None,
            };
            if let Some(bsf) = bsf {
                a.push("-bsf:v".into());
                a.push(format!(
                    "{bsf}=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1"
                ));
            }
        }
    }

    if opts.audio_tail {
        // Match the reference file: track metadata (language, handler names) kept,
        // movie timescale 1000 as written by the ffmpeg build it was made with.
        a.extend(["-movie_timescale", "1000"].map(String::from));
    } else {
        a.extend(["-map_metadata", "-1", "-map_chapters", "-1"].map(String::from));
    }
    a.extend(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"].map(String::from));
    a.push(output.to_string_lossy().into_owned());
    a
}

/// Parses one `-progress` line pair into (out_time seconds, speed).
pub fn parse_progress_line(line: &str, out_time: &mut Option<f64>, speed: &mut Option<f64>) {
    if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
        // Despite the name, out_time_ms is also in microseconds.
        if let Ok(us) = v.trim().parse::<f64>() {
            *out_time = Some(us / 1_000_000.0);
        }
    } else if let Some(v) = line.strip_prefix("speed=") {
        *speed = v.trim().trim_end_matches('x').parse().ok();
    }
}

/// Starts the remux in a background thread; reports via `job-progress`, `job-done`, `job-failed` events.
pub fn start(app: AppHandle, jobs: Jobs, job_id: u64, report: ProbeReport, opts: Options) -> Result<String, String> {
    let ffmpeg = tools::ffmpeg()?;
    let output = output_path(Path::new(&report.path));
    let args = build_args(&report, &output, opts);

    let mut child = tools::command(ffmpeg)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Не удалось запустить ffmpeg: {e}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    jobs.0.lock().unwrap().insert(job_id, child);

    let out_str = output.to_string_lossy().into_owned();
    let duration = report.duration_s * opts.factor() as f64;
    let ret = out_str.clone();

    std::thread::spawn(move || {
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let (mut out_time, mut speed) = (None, None);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            parse_progress_line(&line, &mut out_time, &mut speed);
            if line.starts_with("progress=") {
                let percent = match (out_time, duration > 0.0) {
                    (Some(t), true) => (t / duration * 100.0).clamp(0.0, 100.0),
                    _ => 0.0,
                };
                let _ = app.emit("job-progress", Progress { job_id, percent, speed });
            }
        }

        let status = jobs.0.lock().unwrap().remove(&job_id).map(|mut c| c.wait());
        let stderr_text = err_reader.join().unwrap_or_default();

        // The header patch runs only after ffmpeg has closed the file.
        let patch_err = match &status {
            Some(Ok(s)) if s.success() => post_process(Path::new(&out_str), opts).err(),
            _ => None,
        };
        if let Some(message) = patch_err {
            let _ = std::fs::remove_file(&out_str);
            let _ = app.emit("job-failed", Failed { job_id, message, cancelled: false });
            return;
        }

        match status {
            Some(Ok(s)) if s.success() => {
                let size_bytes = std::fs::metadata(&out_str).map(|m| m.len()).unwrap_or(0);
                let _ = app.emit("job-done", Done { job_id, output: out_str, size_bytes });
            }
            other => {
                let _ = std::fs::remove_file(&out_str);
                // A killed process exits with a non-zero code and no error text.
                let cancelled = matches!(other, Some(Ok(_))) && stderr_text.trim().is_empty();
                let message = if cancelled {
                    "Отменено".to_string()
                } else {
                    let tail: Vec<&str> = stderr_text.lines().rev().take(5).collect();
                    let tail: Vec<&str> = tail.into_iter().rev().collect();
                    format!("ffmpeg завершился с ошибкой: {}", tail.join(" | "))
                };
                let _ = app.emit("job-failed", Failed { job_id, message, cancelled });
            }
        }
    });

    Ok(ret)
}
