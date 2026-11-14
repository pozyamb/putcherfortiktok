//! Audio tail patch: the last four audio frames are overwritten in place,
//! everything else in the file stays byte-identical.

use putcherfortiktok_lib::boxpatch::{self, AUDIO_TAIL_FRAMES, AUDIO_TAIL_PATTERN};
use putcherfortiktok_lib::{probe, remux, tools};

#[test]
fn audio_tail_patch_touches_only_last_frames() {
    let (Ok(ffmpeg), Ok(_)) = (tools::ffmpeg(), tools::ffprobe()) else {
        eprintln!("ffmpeg/ffprobe not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("tail-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.mp4");
    let status = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=540x960:r=60:d=3",
               "-f", "lavfi", "-i", "sine=f=440:d=3", "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-shortest"])
        .arg(&input)
        .status()
        .unwrap();
    assert!(status.success());

    let report = probe::probe(input.to_str().unwrap()).unwrap();
    let opts: remux::Options = serde_json::from_str(r#"{"audioTail":true}"#).unwrap();
    let output = remux::output_path(&input);
    let args = remux::build_args(&report, &output, opts);
    let status = tools::command(&ffmpeg).args(&args).stdout(std::process::Stdio::null()).status().unwrap();
    assert!(status.success());

    let before = std::fs::read(&output).unwrap();
    remux::post_process(&output, opts).unwrap();
    let after = std::fs::read(&output).unwrap();
    assert_eq!(before.len(), after.len());

    // Locate the audio samples from the file's own tables.
    let moov_at = after.windows(4).position(|w| w == b"moov").unwrap();
    let moov_size = u32::from_be_bytes(after[moov_at - 4..moov_at].try_into().unwrap()) as usize;
    let samples = boxpatch::audio_samples(&after[moov_at + 4..moov_at - 4 + moov_size]).unwrap();
    assert!(samples.len() > AUDIO_TAIL_FRAMES);
    let split = samples.len() - AUDIO_TAIL_FRAMES;

    let mut expected = before.clone();
    for &(off, size) in &samples[split..] {
        let (off, size) = (off as usize, size as usize);
        let frame = &after[off..off + size];
        let n = AUDIO_TAIL_PATTERN.len().min(size);
        assert_eq!(&frame[..n], &AUDIO_TAIL_PATTERN[..n]);
        assert!(frame[n..].iter().all(|b| *b == 0));
        expected[off..off + size].copy_from_slice(frame);
    }
    // Nothing outside those frames changed.
    assert!(expected == after);
    // Earlier audio frames are untouched and the file still probes cleanly.
    let (off, size) = samples[split - 1];
    assert_eq!(&before[off as usize..(off + size as u64) as usize], &after[off as usize..(off + size as u64) as usize]);
    let probed = probe::probe(output.to_str().unwrap()).unwrap();
    assert!(probed.audio.is_some());
    assert!((probed.duration_s - report.duration_s).abs() < 0.1);

    std::fs::remove_dir_all(&dir).unwrap();
}
