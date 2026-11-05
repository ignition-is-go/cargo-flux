use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[test]
fn large_archives_download_concurrently_with_pinned_etags() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("flux.toml"), "").unwrap();
    std::fs::create_dir(source.path().join("dist")).unwrap();
    let mut bytes = Vec::with_capacity(80 * 1024 * 1024);
    let mut state = 0x123456789abcdef_u64;
    for _ in 0..(80 * 1024 * 1024 / 8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    std::fs::write(source.path().join("dist/payload"), &bytes).unwrap();
    let saved = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .arg("--root")
        .arg(source.path())
        .args(["storage", "save", "--key", "ranges", "--path", "dist"])
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let archive = std::fs::read_dir(source.path().join(".flux/cache/objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let length = std::fs::metadata(&archive).unwrap().len();
    assert!(length >= 64 * 1024 * 1024);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let maximum = Arc::new(AtomicUsize::new(0));
    let stopped = stop.clone();
    let peak = maximum.clone();
    let server = std::thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        let arrivals = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let mut handlers = Vec::new();
        while !stopped.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut connection, _)) => {
                    let archive = archive.clone();
                    let peak = peak.clone();
                    let active = active.clone();
                    let arrivals = arrivals.clone();
                    handlers.push(std::thread::spawn(move || {
                        connection.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        connection.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                        let mut reader = std::io::BufReader::new(connection.try_clone().unwrap());
                        let mut headers = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 { return; }
                            headers.push_str(&line);
                            if line == "\r\n" { break; }
                        }
                        if headers.starts_with("HEAD ") {
                            write!(connection, "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nETag: \"snapshot-version\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nConnection: close\r\n\r\n").unwrap();
                            return;
                        }
                        let headers = headers.to_ascii_lowercase();
                        assert!(headers.contains("if-match: \"snapshot-version\""), "range is not pinned to one object version");
                        let range = headers.lines().find_map(|line| line.strip_prefix("range: bytes=")).unwrap();
                        let (start,end) = range.split_once('-').unwrap();
                        let start: u64 = start.parse().unwrap(); let end: u64 = end.trim().parse().unwrap();
                        assert!(end-start < 8*1024*1024);
                        let running = active.fetch_add(1, Ordering::SeqCst)+1;
                        peak.fetch_max(running, Ordering::SeqCst);
                        let (lock, changed) = &*arrivals;
                        let mut count = lock.lock().unwrap(); *count += 1; changed.notify_all();
                        let deadline = Instant::now()+Duration::from_secs(2);
                        while *count < 4 {
                            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break; };
                            count = changed.wait_timeout(count, remaining).unwrap().0;
                        }
                        drop(count);
                        write!(connection, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{length}\r\nETag: \"snapshot-version\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nConnection: close\r\n\r\n", end-start+1).unwrap();
                        let mut input = std::fs::File::open(archive).unwrap(); input.seek(SeekFrom::Start(start)).unwrap();
                        std::io::copy(&mut input.take(end-start+1), &mut connection).unwrap();
                        active.fetch_sub(1, Ordering::SeqCst);
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("{error}"),
            }
        }
        for handler in handlers {
            handler.join().unwrap();
        }
    });
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(
        destination.path().join("flux.toml"),
        format!("[cache.s3]\nendpoint={endpoint:?}\nbucket='fixture'\ntimeout_seconds=30\n"),
    )
    .unwrap();
    let restored = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .arg("--root")
        .arg(destination.path())
        .args([
            "storage",
            "restore",
            "--key",
            "ranges",
            "--path",
            "dist",
            "--required",
        ])
        .env("CARGO_FLUX_S3_ACCESS_KEY_ID", "fixture")
        .env("CARGO_FLUX_S3_SECRET_ACCESS_KEY", "fixture")
        .env("CARGO_FLUX_S3_REGION", "us-east-1")
        .env_remove("CARGO_FLUX_S3_SESSION_TOKEN")
        .env("NO_PROXY", "*")
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 4);
    assert_eq!(
        std::fs::read(destination.path().join("dist/payload")).unwrap(),
        bytes
    );
}
