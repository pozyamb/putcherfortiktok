//! File analysis: ffprobe JSON → a report with TikTok-relevant warnings.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::tools;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub path: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub duration_s: f64,
    pub container: String,
    pub overall_bitrate: Option<u64>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// Streams other than the first video + first audio (dropped by remux).
    pub extra_streams: usize,
    /// Some(true) when the `moov` atom precedes `mdat` (fast start); None for non-MP4/MOV.
    pub moov_first: Option<bool>,
    pub notes: Vec<Note>,
    /// Remux (stream copy) produces a TikTok-friendly file without re-encoding.
    pub remux_ok: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    pub codec: String,
    pub profile: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub is_vfr: bool,
    pub pix_fmt: String,
    pub bit_depth: u8,
    pub bitrate: Option<u64>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub bitrate: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    /// "ok" | "info" | "warn" | "error"
    pub level: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl Note {
    fn new(level: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self { level, code, message: message.into() }
    }
}

// ---------- ffprobe JSON ----------

#[derive(Deserialize)]
struct FfprobeOut {
    #[serde(default)]
    streams: Vec<FfStream>,
    format: Option<FfFormat>,
}

#[derive(Deserialize)]
struct FfFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Deserialize)]
struct FfStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    bit_rate: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    color_range: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    #[serde(default)]
    disposition: Option<FfDisposition>,
}

#[derive(Deserialize, Default)]
struct FfDisposition {
    #[serde(default)]
    attached_pic: u8,
}

pub fn probe(path: &str) -> Result<ProbeReport, String> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("Файл не найден: {path}"));
    }
    let ffprobe = tools::ffprobe()?;
    let out = tools::command(ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(p)
        .output()
        .map_err(|e| format!("Не удалось запустить ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ffprobe не смог прочитать файл: {}", err.trim()));
    }
    let parsed: FfprobeOut = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("Неожиданный ответ ffprobe: {e}"))?;

    let size_bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let moov_first = if is_mp4_family(p) { moov_before_mdat(p).ok().flatten() } else { None };
    Ok(build_report(path, size_bytes, parsed, moov_first))
}

fn build_report(path: &str, size_bytes: u64, parsed: FfprobeOut, moov_first: Option<bool>) -> ProbeReport {
    let format = parsed.format;
    let duration_s = format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    let container = format
        .as_ref()
        .and_then(|f| f.format_name.clone())
        .unwrap_or_default();
    let overall_bitrate = format.as_ref().and_then(|f| f.bit_rate.as_deref()).and_then(|b| b.parse().ok());
    let size_bytes = if size_bytes > 0 {
        size_bytes
    } else {
        format.as_ref().and_then(|f| f.size.as_deref()).and_then(|s| s.parse().ok()).unwrap_or(0)
    };

    let mut video = None;
    let mut audio = None;
    let mut extra_streams = 0;
    for s in &parsed.streams {
        let is_cover = s.disposition.as_ref().map(|d| d.attached_pic == 1).unwrap_or(false);
        match s.codec_type.as_deref() {
            Some("video") if video.is_none() && !is_cover => video = Some(video_info(s)),
            Some("audio") if audio.is_none() => audio = Some(audio_info(s)),
            _ => extra_streams += 1,
        }
    }

    let file_name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    let mut report = ProbeReport {
        path: path.to_string(),
        file_name,
        size_bytes,
        duration_s,
        container,
        overall_bitrate,
        video,
        audio,
        extra_streams,
        moov_first,
        notes: Vec::new(),
        remux_ok: false,
    };
    let (notes, remux_ok) = assess(&report);
    report.notes = notes;
    report.remux_ok = remux_ok;
    report
}

fn video_info(s: &FfStream) -> VideoInfo {
    let pix_fmt = s.pix_fmt.clone().unwrap_or_default();
    let avg = s.avg_frame_rate.as_deref().and_then(parse_rate);
    let real = s.r_frame_rate.as_deref().and_then(parse_rate);
    let fps = avg.or(real).unwrap_or(0.0);
    let is_vfr = match (avg, real) {
        (Some(a), Some(r)) if a > 0.0 && r > 0.0 => (a - r).abs() / r > 0.01,
        _ => false,
    };
    VideoInfo {
        codec: s.codec_name.clone().unwrap_or_default(),
        profile: s.profile.clone(),
        width: s.width.unwrap_or(0),
        height: s.height.unwrap_or(0),
        fps,
        is_vfr,
        bit_depth: bit_depth(&pix_fmt, s.bits_per_raw_sample.as_deref()),
        pix_fmt,
        bitrate: s.bit_rate.as_deref().and_then(|b| b.parse().ok()),
        color_primaries: known(s.color_primaries.as_deref()),
        color_transfer: known(s.color_transfer.as_deref()),
        color_space: known(s.color_space.as_deref()),
        color_range: known(s.color_range.as_deref()),
    }
}

fn audio_info(s: &FfStream) -> AudioInfo {
    AudioInfo {
        codec: s.codec_name.clone().unwrap_or_default(),
        sample_rate: s.sample_rate.as_deref().and_then(|r| r.parse().ok()),
        channels: s.channels,
        bitrate: s.bit_rate.as_deref().and_then(|b| b.parse().ok()),
    }
}

/// ffprobe reports missing values as "unknown"/"unspecified"; treat them as absent.
fn known(v: Option<&str>) -> Option<String> {
    match v {
        None | Some("") | Some("unknown") | Some("unspecified") | Some("reserved") => None,
        Some(x) => Some(x.to_string()),
    }
}

/// "60000/1001" → 59.94; "0/0" → None.
pub fn parse_rate(r: &str) -> Option<f64> {
    let (n, d) = r.split_once('/').unwrap_or((r, "1"));
    let n: f64 = n.trim().parse().ok()?;
    let d: f64 = d.trim().parse().ok()?;
    if n <= 0.0 || d <= 0.0 {
        None
    } else {
        Some(n / d)
    }
}

pub fn bit_depth(pix_fmt: &str, bits_per_raw_sample: Option<&str>) -> u8 {
    for (marker, depth) in [("16", 16u8), ("14", 14), ("12", 12), ("10", 10)] {
        if pix_fmt.contains(marker) {
            return depth;
        }
    }
    bits_per_raw_sample.and_then(|b| b.parse().ok()).unwrap_or(8)
}

/// Builds the warning list and decides whether a stream-copy remux is enough.
fn assess(r: &ProbeReport) -> (Vec<Note>, bool) {
    let mut notes = Vec::new();
    let Some(v) = &r.video else {
        notes.push(Note::new("error", "no_video", "В файле нет видеодорожки."));
        return (notes, false);
    };
    let mut remux_ok = true;

    let codec_ok = matches!(v.codec.as_str(), "h264" | "hevc");
    if codec_ok {
        notes.push(Note::new("ok", "codec", format!("Кодек {} подходит для TikTok", codec_label(&v.codec))));
    } else {
        remux_ok = false;
        notes.push(Note::new(
            "warn",
            "codec",
            format!("Кодек {}: TikTok принимает H.264 или HEVC, нужно перекодирование (NVENC)", codec_label(&v.codec)),
        ));
    }

    if v.bit_depth > 8 {
        remux_ok = false;
        notes.push(Note::new("warn", "bit_depth", format!("{}-bit видео: TikTok сведёт его к 8-bit сам и хуже, лучше перекодировать", v.bit_depth)));
    }
    if !v.pix_fmt.is_empty() && !v.pix_fmt.contains("420") {
        remux_ok = false;
        notes.push(Note::new("warn", "chroma", format!("Цветовая субдискретизация {}: нужен формат 4:2:0", v.pix_fmt)));
    }

    match v.color_primaries.as_deref() {
        None => notes.push(Note::new("warn", "color_tags", "Нет цветовых тегов: ремукс пропишет BT.709")),
        Some("bt2020") => {
            remux_ok = false;
            notes.push(Note::new("warn", "hdr", "HDR (BT.2020): TikTok может исказить цвета, лучше перевести в SDR"));
        }
        Some(p) => notes.push(Note::new("ok", "color_tags", format!("Цветовые теги: {p}"))),
    }

    match r.moov_first {
        Some(true) => notes.push(Note::new("ok", "faststart", "moov в начале файла")),
        Some(false) => notes.push(Note::new("warn", "faststart", "moov в конце файла: ремукс перенесёт его в начало")),
        None => {}
    }

    if v.is_vfr {
        notes.push(Note::new("warn", "vfr", "Переменная частота кадров: TikTok может дёргать видео, лучше перекодировать"));
    }
    if v.fps > 60.5 {
        notes.push(Note::new("info", "fps", format!("{:.2} fps: выше 60 TikTok может не показать", v.fps)));
    }

    let (w, h) = (v.width.max(1) as f64, v.height.max(1) as f64);
    if ((w / h) - 9.0 / 16.0).abs() > 0.01 {
        notes.push(Note::new("info", "aspect", format!("Соотношение {}×{} — не 9:16, по бокам будут поля", v.width, v.height)));
    }
    if v.width.min(v.height) < 1080 {
        notes.push(Note::new("info", "resolution", format!("Разрешение ниже 1080p ({}×{})", v.width, v.height)));
    }

    match &r.audio {
        None => notes.push(Note::new("info", "no_audio", "Нет звука: TikTok может отклонить ролик без звука")),
        Some(a) if a.codec != "aac" => notes.push(Note::new("warn", "audio_codec", format!("Звук {}: TikTok лучше принимает AAC", a.codec))),
        Some(_) => {}
    }

    if r.extra_streams > 0 {
        notes.push(Note::new("info", "extra_streams", format!("Лишних дорожек: {} — ремукс их уберёт", r.extra_streams)));
    }

    (notes, remux_ok)
}

fn codec_label(codec: &str) -> String {
    match codec {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "prores" => "ProRes".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        other => other.to_uppercase(),
    }
}

fn is_mp4_family(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mp4" | "mov" | "m4v")
    )
}

/// Scans top-level ISO-BMFF boxes. Some(true) if `moov` comes before `mdat`.
pub fn moov_before_mdat(p: &Path) -> std::io::Result<Option<bool>> {
    let mut f = File::open(p)?;
    let len = f.metadata()?.len();
    scan_boxes(&mut f, len)
}

pub fn scan_boxes<R: Read + Seek>(r: &mut R, len: u64) -> std::io::Result<Option<bool>> {
    let mut pos = 0u64;
    while pos + 8 <= len {
        r.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr)?;
        let mut size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let kind = &hdr[4..8];
        if size == 1 {
            let mut big = [0u8; 8];
            r.read_exact(&mut big)?;
            size = u64::from_be_bytes(big);
        } else if size == 0 {
            size = len - pos;
        }
        match kind {
            b"moov" => return Ok(Some(true)),
            b"mdat" => return Ok(Some(false)),
            _ => {}
        }
        if size < 8 {
            break; // corrupt
        }
        pos += size;
    }
    Ok(None)
}

/// Builds a report from raw ffprobe JSON (used by `probe` and by tests).
pub fn report_from_json(
    path: &str,
    size_bytes: u64,
    json: &[u8],
    moov_first: Option<bool>,
) -> Result<ProbeReport, String> {
    let parsed: FfprobeOut =
        serde_json::from_slice(json).map_err(|e| format!("Неожиданный ответ ffprobe: {e}"))?;
    Ok(build_report(path, size_bytes, parsed, moov_first))
}
