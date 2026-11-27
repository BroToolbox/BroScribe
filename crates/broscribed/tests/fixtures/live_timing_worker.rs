//! Standalone std-only subprocess for live watchdog tests. Compiled before
//! supervision starts so interpreter initialization is not part of its silence.

use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

fn emit(out: &mut impl Write, line: &str) -> io::Result<()> {
    writeln!(out, "{line}")?;
    out.flush()
}

fn ended(eof: &Receiver<io::Result<()>>, period: Duration) -> io::Result<bool> {
    match eof.recv_timeout(period) {
        Ok(result) => result.map(|()| true),
        Err(RecvTimeoutError::Timeout) => Ok(false),
        Err(RecvTimeoutError::Disconnected) => Err(io::Error::other("stdin reader disconnected")),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "warm-up") {
        // Launched once right after compilation, before any timing test.
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "profile-effect") {
        let data = std::env::var_os("BROSCRIBE_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&args[1]));
        let library = args.windows(2).find(|pair| pair[0] == "--library")
            .ok_or_else(|| io::Error::other("missing library"))?;
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&library[1])?;
        std::fs::write(data.join("child-effect"), b"synthetic profile write")?;
        std::fs::write(std::path::Path::new(&library[1]).join("child-effect"), b"synthetic library write")?;
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "stopped") {
        let first = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&args[1])
        {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        emit(&mut io::stdout().lock(), &args[2])?;
        if first {
            // Match a stopped worker: no stdin consumption or output. The
            // watchdog must kill/reap it before starting its successor.
            loop {
                std::thread::park();
            }
        }
        io::copy(&mut io::stdin().lock(), &mut io::sink())?;
        return Ok(());
    }
    let speakers = match args.first().map(String::as_str) {
        Some("loading" | "unready") => false,
        Some("speakers" | "speakers-busy" | "speakers-unready") => true,
        _ => return Err(io::Error::other("unknown timing fixture scenario")),
    };
    let mut input = BufReader::new(io::stdin());
    if speakers {
        // The daemon must send the real SpeakersStart before the child can
        // report model loading or readiness. Keep the remainder for EOF.
        let mut start = String::new();
        if input.read_line(&mut start)? == 0 || !start.ends_with('\n') {
            return Err(io::Error::other("missing SpeakersStart line"));
        }
    }
    let (tx, eof) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let result = io::copy(&mut input, &mut io::sink()).map(|_| ());
        let _ = tx.send(result);
    });
    let mut out = io::stdout().lock();
    if args[0] == "unready" || args[0] == "speakers-unready" {
        loop {
            emit(&mut out, &args[1])?;
            emit(&mut out, &args[2])?;
            if ended(&eof, Duration::from_millis(50))? {
                break;
            }
        }
    } else if speakers {
        let count: usize = args[1].parse().map_err(io::Error::other)?;
        for _ in 0..count {
            emit(&mut out, &args[2])?;
            if ended(&eof, Duration::from_millis(100))? {
                reader.join().expect("stdin reader panicked");
                return Ok(());
            }
        }
        emit(&mut out, &args[3])?;
        let mut busy_ms = 0u64;
        loop {
            if args[0] == "speakers-busy" {
                busy_ms += 100;
                emit(
                    &mut out,
                    &format!(
                        "{{\"type\":\"heartbeat\",\"pending_s\":0.0,\"busy_ms\":{busy_ms}}}"
                    ),
                )?;
            } else {
                emit(&mut out, &args[4])?;
            }
            if ended(&eof, Duration::from_millis(50))? {
                break;
            }
        }
    } else {
        loop {
            emit(&mut out, &args[1])?;
            if ended(&eof, Duration::from_millis(100))? {
                break;
            }
        }
    }
    reader.join().expect("stdin reader panicked");
    Ok(())
}
