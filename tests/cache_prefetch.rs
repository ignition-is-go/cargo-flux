use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[test]
fn sequential_tasks_prefetch_four_cache_entries_before_execution() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stopped = Arc::new(AtomicBool::new(false));
    let maximum = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let arrivals = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let stop = stopped.clone();
    let peak = maximum.clone();
    let worker = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut connection, _)) => {
                    let arrivals = arrivals.clone();
                    let peak = peak.clone();
                    let active = active.clone();
                    handlers.push(std::thread::spawn(move || {
                        connection.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut reader = std::io::BufReader::new(connection.try_clone().unwrap());
                        let mut headers = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 { return; }
                            headers.push_str(&line);
                            if line == "\r\n" { break; }
                        }
                        if headers.starts_with("GET ") {
                            let running = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(running, Ordering::SeqCst);
                            let (lock, changed) = &*arrivals;
                            let mut count = lock.lock().unwrap();
                            *count += 1;
                            changed.notify_all();
                            let deadline = Instant::now() + Duration::from_secs(2);
                            while *count < 4 {
                                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break; };
                                count = changed.wait_timeout(count, remaining).unwrap().0;
                            }
                            active.fetch_sub(1, Ordering::SeqCst);
                            connection.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        } else {
                            let length = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                            }).unwrap_or(0);
                            reader.read_exact(&mut vec![0; length]).unwrap();
                            connection.write_all(b"HTTP/1.1 200 OK\r\nETag: \"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        }
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
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source"), "input").unwrap();
    let mut config =
        format!("[cache.s3]\nendpoint={endpoint:?}\nbucket='fixture'\ntimeout_seconds=5\n");
    for (index, name) in ["one", "two", "three", "four"].iter().enumerate() {
        let dependency = if index == 0 {
            String::new()
        } else {
            format!("depends_on=[{:?}]\n", ["one", "two", "three"][index - 1])
        };
        config.push_str(&format!("[tasks.{name}]\n{dependency}root=['git','--version']\ncache={{inputs=['source'],paths=[]}}\n"));
    }
    std::fs::write(root.path().join("flux.toml"), config).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .arg("--root")
        .arg(root.path())
        .args(["run", "four", "--cache-required"])
        .env("CARGO_FLUX_S3_ACCESS_KEY_ID", "fixture")
        .env("CARGO_FLUX_S3_SECRET_ACCESS_KEY", "fixture")
        .env("CARGO_FLUX_S3_REGION", "us-east-1")
        .env_remove("CARGO_FLUX_S3_SESSION_TOKEN")
        .env("NO_PROXY", "*")
        .output()
        .unwrap();
    stopped.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        maximum.load(Ordering::SeqCst),
        4,
        "cache downloads were serialized"
    );
}
