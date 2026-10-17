//! Parallel downloading for sites that limit each connection.
//!
//! YouTube serves one connection at about 30 Mbit/s but four at once at
//! 200-400 Mbit/s (measured on the Frame), and a 4K stream needs up to
//! 25 Mbit/s, so a single connection leaves no room for hiccups or seeks.
//! `serve` puts a small HTTP server on 127.0.0.1 in front of the stream:
//! ffmpeg and ffplay read from it as usual (with byte ranges for seeking),
//! and it fetches each requested range from the site as several smaller
//! ranges over parallel connections, handing the bytes back in order.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Downloads running at once for one reader.
const PARALLEL: usize = 4;
/// First piece of each request is small so opening and seeking answer
/// quickly; later pieces double up to `MAX_PIECE`.
const FIRST_PIECE: u64 = 256 << 10;
const MAX_PIECE: u64 = 4 << 20;

/// Serve `url` from a local address that downloads it in parallel pieces.
/// Fails when the site doesn't answer byte-range requests (then use `url`
/// directly). The local address keeps `url`'s query, which carries hints
/// such as YouTube's projection tag.
pub fn serve(url: &str) -> Result<String, String> {
    let total = size(url)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no local port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let upstream = Arc::new(url.to_string());
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let upstream = upstream.clone();
            thread::spawn(move || {
                if let Err(e) = answer(conn, &upstream, total) {
                    eprintln!("fetch: {e}");
                }
            });
        }
    });
    let query = url.split_once('?').map_or(String::new(), |(_, q)| format!("?{q}"));
    Ok(format!("http://127.0.0.1:{port}/stream{query}"))
}

/// Total size of `url`, from the answer to a one-byte range request.
fn size(url: &str) -> Result<u64, String> {
    let out = crate::video::output_within(
        Command::new("curl").args(["-sS", "-r", "0-0", "-D", "-", "-o", "/dev/null", "--max-time", "15", url]),
        20,
    )?;
    let head = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    head.lines()
        .find_map(|l| l.strip_prefix("content-range:").and_then(|v| v.trim().rsplit('/').next()?.parse().ok()))
        .ok_or_else(|| "the site doesn't answer byte-range requests".to_string())
}

/// Answer one HTTP request from ffmpeg or ffplay.
fn answer(conn: TcpStream, url: &str, total: u64) -> Result<(), String> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
    let mut request = String::new();
    reader.read_line(&mut request).map_err(|e| e.to_string())?;
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("range")
        {
            range = v.trim().strip_prefix("bytes=").map(str::to_string);
        }
    }
    let head_only = request.starts_with("HEAD");
    let mut out = conn;
    let (from, to) = match &range {
        Some(r) => {
            let (a, b) = r.split_once('-').unwrap_or((r, ""));
            let from: u64 = a.trim().parse().unwrap_or(0);
            let to: u64 = b.trim().parse().map_or(total - 1, |b: u64| b.min(total - 1));
            if from >= total || from > to {
                let _ = write!(
                    out,
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                return Ok(());
            }
            (from, to)
        }
        None => (0, total - 1),
    };
    let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n\
         Content-Range: bytes {from}-{to}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        to - from + 1
    )
    .map_err(|e| e.to_string())?;
    if head_only {
        return Ok(());
    }

    // Pieces download in parallel and are written back in order; when the
    // reader goes away (it seeks by opening a new request) the rest stop.
    let stop = Arc::new(AtomicBool::new(false));
    let mut pending: VecDeque<JoinHandle<Result<Vec<u8>, String>>> = VecDeque::new();
    let mut next = from;
    let mut piece = FIRST_PIECE;
    let result = loop {
        while pending.len() < PARALLEL && next <= to {
            let end = (next + piece - 1).min(to);
            let (url, stop) = (url.to_string(), stop.clone());
            pending.push_back(thread::spawn(move || fetch_piece(&url, next, end, &stop)));
            next = end + 1;
            piece = (piece * 2).min(MAX_PIECE);
        }
        let Some(first) = pending.pop_front() else { break Ok(()) };
        match first.join().unwrap_or_else(|_| Err("download thread failed".into())) {
            Ok(bytes) => {
                if out.write_all(&bytes).is_err() {
                    break Ok(());
                }
            }
            Err(e) => break Err(e),
        }
    };
    stop.store(true, Ordering::Relaxed);
    result
}

/// Bytes `from..=to` of `url`, retried a few times.
fn fetch_piece(url: &str, from: u64, to: u64, stop: &AtomicBool) -> Result<Vec<u8>, String> {
    let want = (to - from + 1) as usize;
    let mut last = String::new();
    for _ in 0..3 {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let mut child = Command::new("curl")
            .args(["-sS", "--fail", "--max-time", "30", "-r", &format!("{from}-{to}"), url])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run curl: {e}"))?;
        let mut pipe = child.stdout.take().unwrap();
        let mut bytes = Vec::with_capacity(want);
        let mut buf = vec![0u8; 64 << 10];
        let mut stopped = false;
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if stop.load(Ordering::Relaxed) {
                stopped = true;
                let _ = child.kill();
                break;
            }
        }
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        let _ = child.wait();
        if stopped {
            return Err("stopped".into());
        }
        if bytes.len() == want {
            return Ok(bytes);
        }
        last = format!("bytes {from}-{to}: got {} of {want} ({})", bytes.len(), err.trim());
    }
    Err(format!("download failed: {last}"))
}
