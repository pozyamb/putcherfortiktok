//! Remux mode: stream copy into a TikTok-friendly MP4 (no re-encoding).
//!
//! - first video + first audio stream only, everything else dropped
//! - global metadata and chapters stripped
//! - BT.709 colour tags written into the bitstream when the source has none
//! - `moov` atom moved to the front (`+faststart`)

use std::collections::{HashMap, HashSet};
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
use crate::enhance::{self, EnhanceOptions};
use crate::tools;

/// Running ffmpeg processes by job id, so they can be cancelled.
#[derive(Default, Clone)]
pub struct Jobs {
    children: Arc<Mutex<HashMap<u64, Child>>>,
    /// Jobs the user cancelled: the next stage must not start.
    cancelled: Arc<Mutex<HashSet<u64>>>,
}

impl Jobs {
    pub fn cancel(&self, id: u64) -> bool {
        self.cancelled.lock().unwrap().insert(id);
        match self.children.lock().unwrap().get_mut(&id) {
            Some(child) => child.kill().is_ok(),
            None => true,
        }
    }

    fn is_cancelled(&self, id: u64) -> bool {
        self.cancelled.lock().unwrap().contains(&id)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job_id: u64,
    pub percent: f64,
    pub speed: Option<f64>,
    pub stage: &'static str,
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

/// Why a stage stopped.
enum StageError {
    Cancelled,
    Failed(String),
}

/// Progress sink of a stage: (percent of the whole job, ffmpeg speed, stage name).
pub type Emit<'a> = &'a dyn Fn(f64, Option<f64>, &'static str);

/// The motion blur stage on its own, without the app window (tests, command line).
/// `frames` is the temporary folder; `output` gets the encoded, not yet patched video.
pub fn blur_to_file(report: &ProbeReport, enh: &EnhanceOptions, frames: &Path, output: &Path, emit: Emit) -> Result<(), String> {
    let result = run_blur(emit, &Jobs::default(), 0, report, enh, frames, output);
    let _ = std::fs::remove_dir_all(frames);
    result.map_err(|e| match e {
        StageError::Cancelled => "cancelled".to_string(),
        StageError::Failed(m) => m,
    })
}

/// Runs one ffmpeg process to completion, forwarding progress mapped into `from..to` percent.
#[allow(clippy::too_many_arguments)]
fn run_stage(
    emit: Emit,
    jobs: &Jobs,
    job_id: u64,
    stage: &'static str,
    args: &[String],
    duration: f64,
    from: f64,
    to: f64,
) -> Result<(), StageError> {
    if jobs.is_cancelled(job_id) {
        return Err(StageError::Cancelled);
    }
    let ffmpeg = tools::ffmpeg().map_err(StageError::Failed)?;
    let mut child = tools::command(ffmpeg)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| StageError::Failed(format!("Не удалось запустить ffmpeg: {e}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    jobs.children.lock().unwrap().insert(job_id, child);

    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let (mut out_time, mut speed) = (None, None);
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        parse_progress_line(&line, &mut out_time, &mut speed);
        if line.starts_with("progress=") {
            let done = match (out_time, duration > 0.0) {
                (Some(t), true) => (t / duration).clamp(0.0, 1.0),
                _ => 0.0,
            };
            let percent = from + (to - from) * done;
            emit(percent, speed, stage);
        }
    }

    let status = jobs.children.lock().unwrap().remove(&job_id).map(|mut c| c.wait());
    let stderr_text = err_reader.join().unwrap_or_default();
    match status {
        Some(Ok(s)) if s.success() => Ok(()),
        _ if jobs.is_cancelled(job_id) => Err(StageError::Cancelled),
        _ => {
            let tail: Vec<&str> = stderr_text.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            Err(StageError::Failed(format!("ffmpeg завершился с ошибкой: {}", tail.join(" | "))))
        }
    }
}

/// Runs RIFE to completion. With `-v` it prints one "... done" line per finished frame,
/// which is the only progress signal it gives.
#[allow(clippy::too_many_arguments)]
fn run_rife(
    emit: Emit,
    jobs: &Jobs,
    job_id: u64,
    stage: &'static str,
    exe: &Path,
    args: &[String],
    frames_out: u64,
    from: f64,
    to: f64,
) -> Result<(), StageError> {
    if jobs.is_cancelled(job_id) {
        return Err(StageError::Cancelled);
    }
    let mut child = tools::command(exe)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| StageError::Failed(format!("Не удалось запустить RIFE: {e}")))?;
    let stderr = child.stderr.take().expect("piped stderr");
    jobs.children.lock().unwrap().insert(job_id, child);

    let mut done = 0u64;
    let mut tail: Vec<String> = Vec::new();
    let mut last_emit = std::time::Instant::now();
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if line.trim_end().ends_with("done") {
            done += 1;
            if last_emit.elapsed().as_millis() >= 250 {
                last_emit = std::time::Instant::now();
                let part = (done as f64 / frames_out.max(1) as f64).clamp(0.0, 1.0);
                emit(from + (to - from) * part, None, stage);
            }
        } else if !line.starts_with('[') {
            tail.push(line);
            if tail.len() > 5 {
                tail.remove(0);
            }
        }
    }

    let status = jobs.children.lock().unwrap().remove(&job_id).map(|mut c| c.wait());
    match status {
        Some(Ok(s)) if s.success() && done > 0 => Ok(()),
        _ if jobs.is_cancelled(job_id) => Err(StageError::Cancelled),
        _ => Err(StageError::Failed(format!("RIFE завершился с ошибкой: {}", tail.join(" | ")))),
    }
}

/// Motion blur stage: source frames → RIFE in chunks → PNG stream into one encoder.
///
/// RIFE only interpolates between frames of one run, so each chunk also gets the
/// first frame of the next chunk and its last `per_source` outputs are dropped:
/// output `i` of a run sits at source position `i / per_source`, so the kept ones
/// join the next chunk seamlessly. Frames are deleted as soon as they are sent,
/// so the disk holds the source frames plus one chunk at a time.
#[allow(clippy::too_many_arguments)]
fn run_blur(
    emit: Emit,
    jobs: &Jobs,
    job_id: u64,
    report: &ProbeReport,
    enh: &EnhanceOptions,
    frames: &Path,
    stage1: &Path,
) -> Result<(), StageError> {
    use std::io::Write;

    let (rife_exe, model) = tools::rife().ok_or_else(|| {
        StageError::Failed("Для размытия движения нужен RIFE: положите rife-ncnn-vulkan в папку rife рядом с приложением.".into())
    })?;
    let io = |e: std::io::Error| StageError::Failed(format!("Ошибка временной папки: {e}"));
    let (dir_src, dir_in) = (frames.join("src"), frames.join("in"));
    let _ = std::fs::remove_dir_all(frames);
    std::fs::create_dir_all(&dir_src).map_err(io)?;

    let args = enhance::extract_frames_args(report, &dir_src);
    run_stage(emit, jobs, job_id, "Извлечение кадров", &args, report.duration_s, 0.0, 10.0)?;
    let total = std::fs::read_dir(&dir_src).map(|d| d.count() as u64).unwrap_or(0);
    if total < 2 {
        return Err(StageError::Failed("Не удалось извлечь кадры из видео.".into()));
    }
    let sfps = report.video.as_ref().map(|v| v.fps).unwrap_or(30.0);
    let plan = enhance::blur_plan(sfps, enh);
    let chunk = enhance::blur_chunk(&plan);
    let per = plan.per_source as u64;
    let gpu = tools::rife_gpu_index(&rife_exe);
    let name = |dir: &Path, n: u64| dir.join(format!("{n:08}.png"));

    let ffmpeg = tools::ffmpeg().map_err(StageError::Failed)?;
    let args = enhance::build_args_blur(report, &plan, stage1, enh);
    let mut encoder = tools::command(ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| StageError::Failed(format!("Не удалось запустить ffmpeg: {e}")))?;
    let stdin = encoder.stdin.take().expect("piped stdin");
    let mut stderr = encoder.stderr.take().expect("piped stderr");
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    // A writer thread streams finished chunks into the encoder while RIFE works on
    // the next one. Two output folders take turns; `freed` says one was emptied.
    // An empty `Failed` message means the encoder closed its input (it exited).
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<(PathBuf, u64)>();
    let (freed_tx, freed_rx) = std::sync::mpsc::channel::<()>();
    let writer_jobs = jobs.clone();
    let writer = std::thread::spawn(move || -> Result<(), StageError> {
        let mut stdin = stdin;
        for (dir, count) in chunk_rx {
            for i in 1..=count {
                if i % 64 == 0 && writer_jobs.is_cancelled(job_id) {
                    return Err(StageError::Cancelled);
                }
                let bytes = std::fs::read(dir.join(format!("{i:08}.png")))
                    .map_err(|e| StageError::Failed(format!("Ошибка временной папки: {e}")))?;
                if stdin.write_all(&bytes).is_err() {
                    return Err(StageError::Failed(String::new()));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            let _ = freed_tx.send(());
        }
        Ok(())
    });

    let produced = (|| -> Result<(), StageError> {
        let mut start = 1u64;
        let mut turn = 0u64;
        while start <= total {
            if jobs.is_cancelled(job_id) {
                return Err(StageError::Cancelled);
            }
            // From the third chunk on, wait until the writer has emptied this folder.
            // If it stopped instead, its own result says why.
            if turn >= 2 && freed_rx.recv().is_err() {
                return Ok(());
            }
            let dir_out = frames.join(format!("out{}", turn % 2));
            let end = (start + chunk - 1).min(total);
            let count = end - start + 1;
            let _ = std::fs::remove_dir_all(&dir_in);
            let _ = std::fs::remove_dir_all(&dir_out);
            std::fs::create_dir_all(&dir_in).map_err(io)?;
            std::fs::create_dir_all(&dir_out).map_err(io)?;
            for k in 0..count {
                std::fs::rename(name(&dir_src, start + k), name(&dir_in, k + 1)).map_err(io)?;
            }
            let rife_out = if end < total {
                let (from, to) = (name(&dir_src, end + 1), name(&dir_in, count + 1));
                if std::fs::hard_link(&from, &to).is_err() {
                    std::fs::copy(&from, &to).map_err(io)?;
                }
                (count + 1) * per
            } else {
                count * per
            };

            let from = 10.0 + 85.0 * (start - 1) as f64 / total as f64;
            let to = 10.0 + 85.0 * end as f64 / total as f64;
            let args = enhance::rife_args(&dir_in, &dir_out, rife_out, &model, gpu);
            run_rife(emit, jobs, job_id, "Размытие движения", &rife_exe, &args, rife_out, from, to)?;
            if chunk_tx.send((dir_out, count * per)).is_err() {
                return Ok(());
            }
            start = end + 1;
            turn += 1;
        }
        Ok(())
    })();
    drop(chunk_tx);
    let written = writer
        .join()
        .unwrap_or_else(|_| Err(StageError::Failed("Сбой при передаче кадров кодировщику.".into())));

    let failure = match (produced, written) {
        (Ok(()), Ok(())) => None,
        (Err(StageError::Cancelled), _) | (_, Err(StageError::Cancelled)) => Some(StageError::Cancelled),
        (_, Err(StageError::Failed(m))) if m.is_empty() => None, // the encoder's exit status tells
        (Err(e), _) | (_, Err(e)) => Some(e),
    };
    if let Some(e) = failure {
        let _ = encoder.kill();
        let _ = encoder.wait();
        return Err(e);
    }

    // Only the encoder's flush is left here: a few seconds at most.
    let status = encoder.wait();
    let text = err_reader.join().unwrap_or_default();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ if jobs.is_cancelled(job_id) => Err(StageError::Cancelled),
        _ => {
            let tail: Vec<&str> = text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
            Err(StageError::Failed(format!("ffmpeg завершился с ошибкой: {}", tail.join(" | "))))
        }
    }
}

/// Temporary folder for PNG frames of the RIFE stage, next to the output file.
pub fn frames_dir(output: &Path) -> PathBuf {
    output.with_extension("frames")
}

/// `clip_tiktok.mp4` → `clip_tiktok.stage1.mp4` (intermediate file of the enhance stage).
pub fn stage1_path(output: &Path) -> PathBuf {
    output.with_extension("stage1.mp4")
}

/// Starts the job in a background thread; reports via `job-progress`, `job-done`, `job-failed`.
///
/// Stages: optional enhancement + NVENC encode, then the remux, then the in-place
/// patches. Whatever is patched is always the final file.
pub fn start(
    app: AppHandle,
    jobs: Jobs,
    job_id: u64,
    report: ProbeReport,
    opts: Options,
    enh: EnhanceOptions,
) -> Result<String, String> {
    tools::ffmpeg()?;
    let output = output_path(Path::new(&report.path));
    let out_str = output.to_string_lossy().into_owned();
    let ret = out_str.clone();

    std::thread::spawn(move || {
        let stage1 = stage1_path(&output);
        let frames = frames_dir(&output);
        let progress = |percent: f64, speed: Option<f64>, stage: &'static str| {
            let _ = app.emit("job-progress", Progress { job_id, percent, speed, stage });
        };
        let emit: Emit = &progress;
        let result = (|| -> Result<(), StageError> {
            let mut source = report.clone();
            let mut remux_from = 0.0;
            if enhance::uses_blur(&report, &enh) {
                run_blur(emit, &jobs, job_id, &report, &enh, &frames, &stage1)?;
                let _ = std::fs::remove_dir_all(&frames);
                source = crate::probe::probe(&stage1.to_string_lossy()).map_err(StageError::Failed)?;
                remux_from = 95.0;
            } else if enhance::uses_rife(&report, &enh) {
                let (rife_exe, model) = tools::rife().ok_or_else(|| {
                    StageError::Failed("RIFE не найден: положите rife-ncnn-vulkan в папку rife рядом с приложением.".into())
                })?;
                let io = |e: std::io::Error| StageError::Failed(format!("Не удалось создать временную папку: {e}"));
                let (dir_in, dir_out) = (frames.join("in"), frames.join("out"));
                let _ = std::fs::remove_dir_all(&frames);
                std::fs::create_dir_all(&dir_in).map_err(io)?;
                std::fs::create_dir_all(&dir_out).map_err(io)?;

                let args = enhance::extract_frames_args(&report, &dir_in);
                run_stage(emit, &jobs, job_id, "Извлечение кадров", &args, report.duration_s, 0.0, 15.0)?;
                let frames_in = std::fs::read_dir(&dir_in).map(|d| d.count() as u64).unwrap_or(0);
                if frames_in < 2 {
                    return Err(StageError::Failed("Не удалось извлечь кадры из видео.".into()));
                }
                let sfps = report.video.as_ref().map(|v| v.fps).unwrap_or(30.0);
                let frames_out = enhance::rife_frame_count(frames_in, sfps, enh.fps_target);
                let gpu = tools::rife_gpu_index(&rife_exe);
                let args = enhance::rife_args(&dir_in, &dir_out, frames_out, &model, gpu);
                run_rife(emit, &jobs, job_id, "Достройка кадров (RIFE)", &rife_exe, &args, frames_out, 15.0, 65.0)?;
                let _ = std::fs::remove_dir_all(&dir_in);

                // Keep the clip length exact: frame rate = produced frames / source duration.
                let fps = if report.duration_s > 0.0 && sfps > 0.0 {
                    frames_out as f64 / (frames_in as f64 / sfps)
                } else {
                    enhance::fps_goal(enh.fps_target) as f64
                };
                let args = enhance::build_args_from_frames(&report, &dir_out, fps, &stage1, &enh);
                run_stage(emit, &jobs, job_id, "Улучшение и кодирование", &args, report.duration_s, 65.0, 95.0)?;
                let _ = std::fs::remove_dir_all(&frames);
                source = crate::probe::probe(&stage1.to_string_lossy()).map_err(StageError::Failed)?;
                remux_from = 95.0;
            } else if enh.enabled {
                let args = enhance::build_args(&report, &stage1, &enh);
                run_stage(emit, &jobs, job_id, "Улучшение и кодирование", &args, report.duration_s, 0.0, 95.0)?;
                source = crate::probe::probe(&stage1.to_string_lossy()).map_err(StageError::Failed)?;
                remux_from = 95.0;
            }
            let args = build_args(&source, &output, opts);
            let duration = source.duration_s * opts.factor() as f64;
            run_stage(emit, &jobs, job_id, "Патч", &args, duration, remux_from, 100.0)?;
            if jobs.is_cancelled(job_id) {
                return Err(StageError::Cancelled);
            }
            post_process(&output, opts).map_err(StageError::Failed)
        })();

        let _ = std::fs::remove_file(&stage1);
        let _ = std::fs::remove_dir_all(&frames);
        jobs.cancelled.lock().unwrap().remove(&job_id);
        match result {
            Ok(()) => {
                let size_bytes = std::fs::metadata(&out_str).map(|m| m.len()).unwrap_or(0);
                let _ = app.emit("job-done", Done { job_id, output: out_str, size_bytes });
            }
            Err(e) => {
                let _ = std::fs::remove_file(&out_str);
                let (message, cancelled) = match e {
                    StageError::Cancelled => ("Отменено".to_string(), true),
                    StageError::Failed(m) => (m, false),
                };
                let _ = app.emit("job-failed", Failed { job_id, message, cancelled });
            }
        }
    });

    Ok(ret)
}
