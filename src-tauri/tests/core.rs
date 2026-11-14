//! Tests for file analysis and remux argument building.
//! Lives in `tests/` (not inline) so build.rs can link the Windows manifest into the test exe.

use std::io::Cursor;
use std::path::Path;

use putcherfortiktok_lib::probe::{self, ProbeReport};
use putcherfortiktok_lib::{remux, tools};

fn boxed(kind: &[u8; 4], payload: usize) -> Vec<u8> {
    let mut v = ((8 + payload) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend(std::iter::repeat(0u8).take(payload));
    v
}

#[test]
fn detects_moov_position() {
    let mut fast = boxed(b"ftyp", 16);
    fast.extend(boxed(b"moov", 40));
    fast.extend(boxed(b"mdat", 100));
    let len = fast.len() as u64;
    assert_eq!(probe::scan_boxes(&mut Cursor::new(fast), len).unwrap(), Some(true));

    let mut slow = boxed(b"ftyp", 16);
    slow.extend(boxed(b"mdat", 100));
    slow.extend(boxed(b"moov", 40));
    let len = slow.len() as u64;
    assert_eq!(probe::scan_boxes(&mut Cursor::new(slow), len).unwrap(), Some(false));
}

#[test]
fn parses_rates_and_depth() {
    assert!((probe::parse_rate("60000/1001").unwrap() - 59.94).abs() < 0.01);
    assert_eq!(probe::parse_rate("0/0"), None);
    assert_eq!(probe::parse_rate("30"), Some(30.0));
    assert_eq!(probe::bit_depth("yuv420p10le", None), 10);
    assert_eq!(probe::bit_depth("yuv420p", Some("8")), 8);
    assert_eq!(probe::bit_depth("yuv422p", None), 8);
}

fn report_from(json: &str, moov_first: Option<bool>) -> ProbeReport {
    probe::report_from_json("C:/v/clip.mov", 1000, json.as_bytes(), moov_first).unwrap()
}

const H264_8BIT: &str = r#"{"streams":[
    {"codec_type":"video","codec_name":"h264","width":1080,"height":1920,"pix_fmt":"yuv420p",
     "r_frame_rate":"60/1","avg_frame_rate":"60/1"},
    {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}],
   "format":{"format_name":"mov,mp4","duration":"12.5"}}"#;

#[test]
fn h264_8bit_is_remuxable() {
    let r = report_from(H264_8BIT, Some(false));
    assert!(r.remux_ok);
    assert_eq!(r.file_name, "clip.mov");
    assert!(r.notes.iter().any(|n| n.code == "faststart" && n.level == "warn"));
    assert!(r.notes.iter().any(|n| n.code == "color_tags" && n.level == "warn"));
    assert!(!r.notes.iter().any(|n| n.code == "aspect"));
}

#[test]
fn hevc_10bit_needs_reencode() {
    let r = report_from(
        r#"{"streams":[
            {"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,"pix_fmt":"yuv420p10le",
             "r_frame_rate":"60000/1001","avg_frame_rate":"60000/1001","color_primaries":"bt709"}],
           "format":{"format_name":"mov,mp4","duration":"24.7"}}"#,
        Some(true),
    );
    assert!(!r.remux_ok);
    assert_eq!(r.video.as_ref().unwrap().bit_depth, 10);
    assert!(r.notes.iter().any(|n| n.code == "bit_depth"));
    assert!(r.notes.iter().any(|n| n.code == "aspect"));
    assert!(r.notes.iter().any(|n| n.code == "no_audio"));
}

#[test]
fn cover_art_is_not_the_video() {
    let r = report_from(
        r#"{"streams":[
            {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
            {"codec_type":"video","codec_name":"h264","width":1080,"height":1920,"pix_fmt":"yuv420p",
             "r_frame_rate":"30/1","avg_frame_rate":"30/1"}],
           "format":{"duration":"5"}}"#,
        None,
    );
    assert_eq!(r.video.as_ref().unwrap().codec, "h264");
    assert_eq!(r.extra_streams, 1);
}

#[test]
fn remux_args_add_color_bsf_only_when_tags_missing() {
    let out = Path::new("C:/v/clip_tiktok.mp4");
    let a = remux::build_args(&report_from(H264_8BIT, Some(false)), out, Default::default());
    assert!(a.iter().any(|x| x.starts_with("h264_metadata=colour_primaries=1")));
    assert!(a.contains(&"+faststart".to_string()));
    assert_eq!(a.last().unwrap(), "C:/v/clip_tiktok.mp4");

    let tagged = H264_8BIT.replace(r#""pix_fmt":"yuv420p","#, r#""pix_fmt":"yuv420p","color_primaries":"bt709","#);
    let a = remux::build_args(&report_from(&tagged, Some(true)), out, Default::default());
    assert!(!a.contains(&"-bsf:v".to_string()));
}

#[test]
fn parses_progress() {
    let (mut t, mut s) = (None, None);
    remux::parse_progress_line("out_time_us=2500000", &mut t, &mut s);
    remux::parse_progress_line("speed=12.3x", &mut t, &mut s);
    assert_eq!(t, Some(2.5));
    assert_eq!(s, Some(12.3));
    remux::parse_progress_line("speed=N/A", &mut t, &mut s);
    assert_eq!(s, None);
}

#[test]
fn output_name_avoids_collisions() {
    let dir = std::env::temp_dir().join(format!("remux-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("clip.mov");
    assert_eq!(remux::output_path(&input), dir.join("clip_tiktok.mp4"));
    std::fs::write(dir.join("clip_tiktok.mp4"), b"x").unwrap();
    assert_eq!(remux::output_path(&input), dir.join("clip_tiktok (2).mp4"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// End to end with the real ffmpeg: a clip with moov at the end and no colour tags
/// comes out with moov first and BT.709 tags. Skipped when ffmpeg is not installed.
#[test]
fn remux_fixes_real_file() {
    let (Ok(ffmpeg), Ok(_)) = (tools::ffmpeg(), tools::ffprobe()) else {
        eprintln!("ffmpeg/ffprobe not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("remux-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.mp4");

    let status = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=1080x1920:r=30:d=2",
               "-f", "lavfi", "-i", "sine=f=440:d=2", "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-shortest", "-metadata", "title=junk"])
        .arg(&input)
        .status()
        .unwrap();
    assert!(status.success());

    let before = probe::probe(input.to_str().unwrap()).unwrap();
    assert_eq!(before.moov_first, Some(false));
    assert!(before.video.as_ref().unwrap().color_primaries.is_none());
    assert!(before.remux_ok);

    let output = remux::output_path(&input);
    let args = remux::build_args(&before, &output, Default::default());
    let status = tools::command(&ffmpeg).args(&args).stdout(std::process::Stdio::null()).status().unwrap();
    assert!(status.success());

    let after = probe::probe(output.to_str().unwrap()).unwrap();
    assert_eq!(after.moov_first, Some(true));
    assert_eq!(after.video.as_ref().unwrap().color_primaries.as_deref(), Some("bt709"));
    assert!(after.audio.is_some());
    assert!((after.duration_s - before.duration_s).abs() < 0.1);

    std::fs::remove_dir_all(&dir).unwrap();
}
