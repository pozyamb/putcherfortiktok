//! In-place edits of MP4 header boxes (no bytes are inserted, sizes never change).
//!
//! EXPERIMENTAL. Used together with the retime pass (`-itsscale N`): the file then
//! declares 1/N of the real frame rate and plays in slow motion locally. These patches
//! write a playback-rate hint of N× into the header so a player that honours it restores
//! normal speed:
//!   - `Mvhd`: movie header "preferred rate" (16.16 fixed point)
//!   - `Elst`: video edit-list media rate, with the segment duration divided by N
//! Whether TikTok honours either is unverified; it has to be A/B tested on real uploads.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RatePatch {
    #[default]
    None,
    Mvhd,
    Elst,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Patched {
    pub mvhd: bool,
    pub elst_entries: usize,
}

struct BoxRef {
    kind: [u8; 4],
    body: usize,
    end: usize,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Child boxes of `b[start..end]`.
fn children(b: &[u8], start: usize, end: usize) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let mut size = u32_at(b, pos) as usize;
        let mut hdr = 8;
        if size == 1 {
            if pos + 16 > end {
                break;
            }
            size = u64_at(b, pos + 8) as usize;
            hdr = 16;
        } else if size == 0 {
            size = end - pos;
        }
        if size < hdr || pos + size > end {
            break;
        }
        out.push(BoxRef { kind: b[pos + 4..pos + 8].try_into().unwrap(), body: pos + hdr, end: pos + size });
        pos += size;
    }
    out
}

fn find<'a>(boxes: &'a [BoxRef], kind: &[u8; 4]) -> Option<&'a BoxRef> {
    boxes.iter().find(|x| &x.kind == kind)
}

/// Patches the payload of a `moov` box in memory.
pub fn patch_moov(moov: &mut [u8], factor: u32, mode: RatePatch) -> Result<Patched, String> {
    if !(1..=16).contains(&factor) {
        return Err(format!("Недопустимый коэффициент: {factor}"));
    }
    let mut done = Patched::default();
    let top = children(moov, 0, moov.len());

    match mode {
        RatePatch::None => {}
        RatePatch::Mvhd => {
            let mvhd = find(&top, b"mvhd").ok_or("В файле нет бокса mvhd")?;
            // FullBox header (4) + v0: 4×u32 / v1: u64,u64,u32,u64 → rate
            let off = mvhd.body + if moov[mvhd.body] == 1 { 32 } else { 20 };
            if off + 4 > mvhd.end {
                return Err("Бокс mvhd повреждён".into());
            }
            moov[off..off + 4].copy_from_slice(&(factor << 16).to_be_bytes());
            done.mvhd = true;
        }
        RatePatch::Elst => {
            for trak in top.iter().filter(|x| &x.kind == b"trak") {
                let tb = children(moov, trak.body, trak.end);
                let is_video = find(&tb, b"mdia")
                    .map(|m| children(moov, m.body, m.end))
                    .and_then(|mb| find(&mb, b"hdlr").map(|h| h.body))
                    // hdlr: FullBox(4) + pre_defined(4) + handler_type(4)
                    .map(|h| moov.get(h + 8..h + 12) == Some(b"vide"))
                    .unwrap_or(false);
                if !is_video {
                    continue;
                }
                let Some(edts) = find(&tb, b"edts") else { continue };
                let eb = children(moov, edts.body, edts.end);
                let Some(elst) = find(&eb, b"elst") else { continue };
                let v1 = moov[elst.body] == 1;
                let count = u32_at(moov, elst.body + 4) as usize;
                let entry = if v1 { 20 } else { 12 };
                for i in 0..count {
                    let e = elst.body + 8 + i * entry;
                    if e + entry > elst.end {
                        break;
                    }
                    let (empty, rate_off) = if v1 {
                        (u64_at(moov, e + 8) == u64::MAX, e + 16)
                    } else {
                        (u32_at(moov, e + 4) == u32::MAX, e + 8)
                    };
                    if empty {
                        continue; // an empty edit is a delay, not media
                    }
                    if v1 {
                        let d = u64_at(moov, e) / factor as u64;
                        moov[e..e + 8].copy_from_slice(&d.to_be_bytes());
                    } else {
                        let d = u32_at(moov, e) / factor;
                        moov[e..e + 4].copy_from_slice(&d.to_be_bytes());
                    }
                    moov[rate_off..rate_off + 2].copy_from_slice(&(factor as u16).to_be_bytes());
                    moov[rate_off + 2..rate_off + 4].copy_from_slice(&[0, 0]);
                    done.elst_entries += 1;
                }
            }
            if done.elst_entries == 0 {
                return Err("У видеодорожки нет списка правок (elst): этот вариант патча неприменим".into());
            }
        }
    }
    Ok(done)
}

/// Finds `moov` in the file, patches it and writes it back in place.
pub fn patch_file(path: &Path, factor: u32, mode: RatePatch) -> Result<Patched, String> {
    if mode == RatePatch::None {
        return Ok(Patched::default());
    }
    let io = |e: std::io::Error| format!("Не удалось изменить файл: {e}");
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(io)?;
    let len = f.metadata().map_err(io)?.len();

    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos)).map_err(io)?;
        let mut hdr = [0u8; 16];
        f.read_exact(&mut hdr[..8]).map_err(io)?;
        let mut size = u32_at(&hdr, 0) as u64;
        let mut hdr_len = 8u64;
        if size == 1 {
            f.read_exact(&mut hdr[8..]).map_err(io)?;
            size = u64_at(&hdr, 8);
            hdr_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hdr_len || pos + size > len {
            break;
        }
        if &hdr[4..8] == b"moov" {
            let mut moov = vec![0u8; (size - hdr_len) as usize];
            f.read_exact(&mut moov).map_err(io)?;
            let done = patch_moov(&mut moov, factor, mode)?;
            f.seek(SeekFrom::Start(pos + hdr_len)).map_err(io)?;
            f.write_all(&moov).map_err(io)?;
            return Ok(done);
        }
        pos += size;
    }
    Err("В файле нет бокса moov".into())
}

// ---------- audio tail patch ----------

/// Bytes found at the start of each of the last four audio frames in a file produced
/// by the reference web optimizer (November 2026); the rest of every frame was zeros.
/// The frame sizes and all sample tables were left untouched.
pub const AUDIO_TAIL_PATTERN: [u8; 15] =
    [0xa0, 0xa0, 0x00, 0x40, 0x01, 0x28, 0x00, 0x64, 0x00, 0x00, 0x03, 0x50, 0x00, 0x00, 0x38];

/// How many trailing audio frames the reference file had overwritten.
pub const AUDIO_TAIL_FRAMES: usize = 4;

/// (file offset, size) of every sample of the first audio track, from a `moov` payload.
pub fn audio_samples(moov: &[u8]) -> Result<Vec<(u64, u32)>, String> {
    let top = children(moov, 0, moov.len());
    for trak in top.iter().filter(|x| &x.kind == b"trak") {
        let tb = children(moov, trak.body, trak.end);
        let Some(mdia) = find(&tb, b"mdia") else { continue };
        let mb = children(moov, mdia.body, mdia.end);
        let is_audio = find(&mb, b"hdlr").map(|h| moov.get(h.body + 8..h.body + 12) == Some(b"soun")).unwrap_or(false);
        if !is_audio {
            continue;
        }
        let minf = find(&mb, b"minf").ok_or("Аудиодорожка повреждена (нет minf)")?;
        let nb = children(moov, minf.body, minf.end);
        let stbl = find(&nb, b"stbl").ok_or("Аудиодорожка повреждена (нет stbl)")?;
        let sb = children(moov, stbl.body, stbl.end);
        let stsz = find(&sb, b"stsz").ok_or("Аудиодорожка повреждена (нет stsz)")?;
        let stsc = find(&sb, b"stsc").ok_or("Аудиодорожка повреждена (нет stsc)")?;
        let (chunks, wide) = match (find(&sb, b"stco"), find(&sb, b"co64")) {
            (Some(c), _) => (c, false),
            (None, Some(c)) => (c, true),
            _ => return Err("Аудиодорожка повреждена (нет stco)".into()),
        };
        let bad = || "Таблицы аудиодорожки повреждены".to_string();
        let need = |b: &BoxRef, n: usize| if b.body + n <= b.end { Ok(()) } else { Err(bad()) };

        need(stsz, 12)?;
        let uniform = u32_at(moov, stsz.body + 4);
        let n_samples = u32_at(moov, stsz.body + 8) as usize;
        if uniform == 0 {
            need(stsz, 12 + n_samples * 4)?;
        }
        need(stsc, 8)?;
        let n_sc = u32_at(moov, stsc.body + 4) as usize;
        need(stsc, 8 + n_sc * 12)?;
        need(chunks, 8)?;
        let n_chunks = u32_at(moov, chunks.body + 4) as usize;
        need(chunks, 8 + n_chunks * if wide { 8 } else { 4 })?;
        if n_sc == 0 {
            return Err(bad());
        }

        let mut out = Vec::with_capacity(n_samples);
        let mut entry = 0;
        for chunk in 1..=n_chunks {
            while entry + 1 < n_sc && u32_at(moov, stsc.body + 8 + (entry + 1) * 12) as usize <= chunk {
                entry += 1;
            }
            let per_chunk = u32_at(moov, stsc.body + 8 + entry * 12 + 4) as usize;
            let mut off = if wide {
                u64_at(moov, chunks.body + 8 + (chunk - 1) * 8)
            } else {
                u32_at(moov, chunks.body + 8 + (chunk - 1) * 4) as u64
            };
            for _ in 0..per_chunk {
                if out.len() == n_samples {
                    break;
                }
                let size = if uniform != 0 { uniform } else { u32_at(moov, stsz.body + 12 + out.len() * 4) };
                out.push((off, size));
                off += size as u64;
            }
        }
        return Ok(out);
    }
    Err("В файле нет аудиодорожки: этот способ неприменим".into())
}

/// Reads the payload of the top-level `moov` box.
fn read_moov(f: &mut std::fs::File) -> Result<Vec<u8>, String> {
    let io = |e: std::io::Error| format!("Не удалось прочитать файл: {e}");
    let len = f.metadata().map_err(io)?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos)).map_err(io)?;
        let mut hdr = [0u8; 16];
        f.read_exact(&mut hdr[..8]).map_err(io)?;
        let mut size = u32_at(&hdr, 0) as u64;
        let mut hdr_len = 8u64;
        if size == 1 {
            f.read_exact(&mut hdr[8..]).map_err(io)?;
            size = u64_at(&hdr, 8);
            hdr_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hdr_len || pos + size > len {
            break;
        }
        if &hdr[4..8] == b"moov" {
            let mut moov = vec![0u8; (size - hdr_len) as usize];
            f.read_exact(&mut moov).map_err(io)?;
            return Ok(moov);
        }
        pos += size;
    }
    Err("В файле нет бокса moov".into())
}

/// Overwrites the last `frames` audio frames in place: `AUDIO_TAIL_PATTERN`, then zeros,
/// keeping every frame's size. Returns how many frames were rewritten.
pub fn patch_audio_tail(path: &Path, frames: usize) -> Result<usize, String> {
    let io = |e: std::io::Error| format!("Не удалось изменить файл: {e}");
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(io)?;
    let len = f.metadata().map_err(io)?.len();
    let moov = read_moov(&mut f)?;
    let samples = audio_samples(&moov)?;
    let tail = &samples[samples.len().saturating_sub(frames)..];
    if tail.is_empty() {
        return Err("В аудиодорожке нет кадров".into());
    }
    for &(off, size) in tail {
        if off + size as u64 > len {
            return Err("Таблицы аудиодорожки указывают за пределы файла".into());
        }
        let mut frame = vec![0u8; size as usize];
        let n = AUDIO_TAIL_PATTERN.len().min(frame.len());
        frame[..n].copy_from_slice(&AUDIO_TAIL_PATTERN[..n]);
        f.seek(SeekFrom::Start(off)).map_err(io)?;
        f.write_all(&frame).map_err(io)?;
    }
    Ok(tail.len())
}
