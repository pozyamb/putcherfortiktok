//! Compares the MP4 box structure of two files and prints what differs.
//!
//!   cargo run --example boxdiff -- original.mp4 processed.mp4
//!
//! Use it on a clip before and after an external "optimizer" to see exactly which
//! header boxes it rewrites. Media data (`mdat`) is compared by size and hash only.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const CONTAINERS: [&[u8; 4]; 9] =
    [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"udta", b"dinf", b"mvex"];

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, x| (h ^ *x as u64).wrapping_mul(0x100000001b3))
}

fn describe(body: &[u8]) -> String {
    if body.len() <= 160 {
        format!("{} bytes: {}", body.len(), hex(body))
    } else {
        format!("{} bytes, hash {:016x}, starts {}", body.len(), fnv(body), hex(&body[..32]))
    }
}

/// Walks boxes in `b`, adding "path -> description" lines.
fn walk(b: &[u8], path: &str, out: &mut BTreeMap<String, String>) {
    let mut pos = 0;
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    while pos + 8 <= b.len() {
        let mut size = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        let mut hdr = 8;
        if size == 1 && pos + 16 <= b.len() {
            size = u64::from_be_bytes(b[pos + 8..pos + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = b.len() - pos;
        }
        if size < hdr || pos + size > b.len() {
            out.insert(format!("{path}/?@{pos}"), "unparsable tail".into());
            break;
        }
        let kind = String::from_utf8_lossy(&b[pos + 4..pos + 8]).into_owned();
        let n = seen.entry(kind.clone()).or_insert(0);
        let p = format!("{path}/{kind}[{n}]");
        *n += 1;
        let body = &b[pos + hdr..pos + size];
        if CONTAINERS.iter().any(|c| c.as_slice() == kind.as_bytes()) {
            out.insert(p.clone(), format!("container, {} bytes", body.len()));
            walk(body, &p, out);
        } else {
            out.insert(p, describe(body));
        }
        pos += size;
    }
}

fn load(path: &str) -> std::io::Result<BTreeMap<String, String>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut out = BTreeMap::new();
    let mut pos = 0u64;
    let mut order = Vec::new();
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let mut hdr = 8u64;
        if size == 1 {
            f.read_exact(&mut h[8..])?;
            size = u64::from_be_bytes(h[8..].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hdr || pos + size > len {
            break;
        }
        let kind = String::from_utf8_lossy(&h[4..8]).into_owned();
        order.push(kind.clone());
        if kind == "mdat" {
            // Hash in chunks: media data can be gigabytes.
            let mut left = size - hdr;
            let mut buf = vec![0u8; 1 << 20];
            let mut hash = 0xcbf29ce484222325u64;
            while left > 0 {
                let n = (left as usize).min(buf.len());
                f.read_exact(&mut buf[..n])?;
                hash = buf[..n].iter().fold(hash, |a, x| (a ^ *x as u64).wrapping_mul(0x100000001b3));
                left -= n as u64;
            }
            out.insert("/mdat".into(), format!("{} bytes, hash {hash:016x}", size - hdr));
        } else {
            let mut body = vec![0u8; (size - hdr) as usize];
            f.read_exact(&mut body)?;
            if kind == "moov" {
                out.insert("/moov".into(), format!("container, {} bytes", body.len()));
                walk(&body, "/moov", &mut out);
            } else {
                out.insert(format!("/{kind}"), describe(&body));
            }
        }
        pos += size;
    }
    out.insert("(top-level order)".into(), order.join(" "));
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: boxdiff <original.mp4> <processed.mp4>");
        std::process::exit(2);
    }
    let a = load(&args[0]).expect("cannot read first file");
    let b = load(&args[1]).expect("cannot read second file");
    let mut same = 0;
    for key in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))) {
        match (a.get(key), b.get(key)) {
            (Some(x), Some(y)) if x == y => same += 1,
            (Some(x), Some(y)) => println!("CHANGED {key}\n   was: {x}\n   now: {y}"),
            (Some(x), None) => println!("REMOVED {key}\n   was: {x}"),
            (None, Some(y)) => println!("ADDED   {key}\n   now: {y}"),
            (None, None) => {}
        }
    }
    println!("{same} boxes identical");
}
