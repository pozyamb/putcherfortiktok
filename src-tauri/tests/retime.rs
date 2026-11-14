//! Experimental retime (`-itsscale`) and header playback-rate patch.

use putcherfortiktok_lib::boxpatch::{self, RatePatch};
use putcherfortiktok_lib::{probe, remux, tools};

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn sample_moov() -> Vec<u8> {
    // mvhd v0: flags, ctime, mtime, timescale=1000, duration=4000, rate=1.0, rest zero
    let mut mvhd = vec![0u8; 100];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&4000u32.to_be_bytes());
    mvhd[20..24].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    let mut hdlr = vec![0u8; 24];
    hdlr[8..12].copy_from_slice(b"vide");
    // elst v0, 2 entries: an empty edit (media_time = -1), then the media
    let mut elst = vec![0, 0, 0, 0, 0, 0, 0, 2];
    for (dur, time) in [(100u32, u32::MAX), (4000, 0)] {
        elst.extend(dur.to_be_bytes());
        elst.extend(time.to_be_bytes());
        elst.extend([0, 1, 0, 0]);
    }
    let trak = [bx(b"edts", &bx(b"elst", &elst)), bx(b"mdia", &bx(b"hdlr", &hdlr))].concat();
    [bx(b"mvhd", &mvhd), bx(b"trak", &trak)].concat()
}

#[test]
fn patches_mvhd_rate_in_place() {
    let mut moov = sample_moov();
    let before = moov.len();
    let done = boxpatch::patch_moov(&mut moov, 2, RatePatch::Mvhd).unwrap();
    assert!(done.mvhd);
    assert_eq!(moov.len(), before);
    assert_eq!(&moov[8 + 20..8 + 24], &0x0002_0000u32.to_be_bytes());
}

#[test]
fn patches_video_elst_rate_and_skips_empty_edit() {
    let mut moov = sample_moov();
    let done = boxpatch::patch_moov(&mut moov, 4, RatePatch::Elst).unwrap();
    assert_eq!(done.elst_entries, 1);
    let pos = moov.windows(4).position(|w| w == b"elst").unwrap() + 4 + 8;
    // first entry (empty edit) untouched
    assert_eq!(&moov[pos..pos + 12], &[0, 0, 0, 100, 255, 255, 255, 255, 0, 1, 0, 0]);
    // second: duration 4000/4, rate 4.0
    assert_eq!(&moov[pos + 12..pos + 24], &[0, 0, 3, 232, 0, 0, 0, 0, 0, 4, 0, 0]);
    assert!(boxpatch::patch_moov(&mut sample_moov(), 99, RatePatch::Mvhd).is_err());
}

/// Real ffmpeg: a 60 fps clip retimed 2x declares 30 fps and twice the video length,
/// the audio survives, and the mvhd rate hint lands in the file.
#[test]
fn retime_halves_declared_fps() {
    let (Ok(ffmpeg), Ok(_)) = (tools::ffmpeg(), tools::ffprobe()) else {
        eprintln!("ffmpeg/ffprobe not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("retime-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.mp4");
    let status = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=1080x1920:r=60:d=2",
               "-f", "lavfi", "-i", "sine=f=440:d=2", "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-shortest"])
        .arg(&input)
        .status()
        .unwrap();
    assert!(status.success());

    let before = probe::probe(input.to_str().unwrap()).unwrap();
    let opts: remux::Options = serde_json::from_str(r#"{"retimeFactor":2,"ratePatch":"mvhd"}"#).unwrap();
    let output = remux::output_path(&input);
    let args = remux::build_args(&before, &output, opts);
    let status = tools::command(&ffmpeg).args(&args).stdout(std::process::Stdio::null()).status().unwrap();
    assert!(status.success());
    let done = boxpatch::patch_file(&output, 2, RatePatch::Mvhd).unwrap();
    assert!(done.mvhd);

    let after = probe::probe(output.to_str().unwrap()).unwrap();
    let fps = after.video.as_ref().unwrap().fps;
    assert!((fps - 30.0).abs() < 0.5, "declared fps {fps}");
    assert!((after.duration_s - before.duration_s * 2.0).abs() < 0.2, "duration {}", after.duration_s);
    assert!(after.audio.is_some());
    assert_eq!(after.moov_first, Some(true));

    std::fs::remove_dir_all(&dir).unwrap();
}
