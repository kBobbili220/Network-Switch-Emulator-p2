//! Buffered writer for `.simlog` files.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use super::schema::{LogEvent, LogFrame, LOG_MAGIC, LOG_VERSION};

pub struct EventLogger {
    file: BufWriter<File>,
    buf: Vec<u8>,
}

impl EventLogger {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = BufWriter::new(File::create(path)?);
        let mut me = Self { file, buf: Vec::with_capacity(256) };
        me.log(0, LogEvent::Header {
            magic: LOG_MAGIC,
            version: LOG_VERSION,
        });
        Ok(me)
    }

    pub fn log(&mut self, at_ns: u64, event: LogEvent) {
        self.buf.clear();
        let frame = LogFrame { at_ns, event };
        let bytes = match postcard::to_allocvec(&frame) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("sim_log: encode error: {e}");
                return;
            }
        };
        let len = bytes.len() as u32;
        if let Err(e) = self.file.write_all(&len.to_le_bytes()) {
            eprintln!("sim_log: write error: {e}");
            return;
        }
        if let Err(e) = self.file.write_all(&bytes) {
            eprintln!("sim_log: write error: {e}");
        }
        let _ = &mut self.buf;
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

impl Drop for EventLogger {
    fn drop(&mut self) {
        self.flush();
    }
}

// ---- Streaming reader (used by the viewer & tests) ----

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Decode(String),
    UnexpectedEof,
    BadHeader,
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self { ReadError::Io(e) }
}

/// Read every frame from a byte slice (the viewer hands us the whole
/// file as bytes; the simulator side uses this in tests).
pub fn decode_all(mut bytes: &[u8]) -> Result<Vec<LogFrame>, ReadError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(ReadError::UnexpectedEof);
        }
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        bytes = &bytes[4..];
        if bytes.len() < len {
            return Err(ReadError::UnexpectedEof);
        }
        let frame: LogFrame = postcard::from_bytes(&bytes[..len])
            .map_err(|e| ReadError::Decode(e.to_string()))?;
        bytes = &bytes[len..];
        out.push(frame);
    }
    if out.is_empty() {
        return Err(ReadError::BadHeader);
    }
    if !matches!(out[0].event, LogEvent::Header { magic: LOG_MAGIC, version: LOG_VERSION }) {
        return Err(ReadError::BadHeader);
    }
    Ok(out)
}

/// Stream frames from a `.simlog` file without holding the whole file in
/// memory. A 60-second graded run is a few hundred megabytes of frames;
/// the scorer makes two passes over one of those and must not need the
/// file resident twice.
pub fn for_each_frame<F>(path: &Path, mut f: F) -> Result<(), ReadError>
where
    F: FnMut(LogFrame),
{
    use std::io::{BufReader, Read};

    let mut rd = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut len_buf = [0u8; 4];
    let mut body = Vec::with_capacity(256);
    let mut first = true;
    loop {
        match rd.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(ReadError::Io(e)),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        body.resize(len, 0);
        rd.read_exact(&mut body)
            .map_err(|_| ReadError::UnexpectedEof)?;
        let frame: LogFrame =
            postcard::from_bytes(&body).map_err(|e| ReadError::Decode(e.to_string()))?;
        if first {
            if !matches!(
                frame.event,
                LogEvent::Header {
                    magic: LOG_MAGIC,
                    version: LOG_VERSION
                }
            ) {
                return Err(ReadError::BadHeader);
            }
            first = false;
            continue;
        }
        f(frame);
    }
    if first {
        return Err(ReadError::BadHeader);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.simlog");
        {
            let mut log = EventLogger::create(&path).unwrap();
            log.log(100, LogEvent::AppAdded { id: 1, ip: 0xc0a80001, dst_ip: 0xc0a80002 });
            log.log(200, LogEvent::PacketDelivered { app: 1, packet_id: 42 });
        }
        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        let frames = decode_all(&bytes).unwrap();
        assert_eq!(frames.len(), 3); // header + 2 events
        assert!(matches!(frames[0].event, LogEvent::Header { .. }));
        assert!(matches!(frames[1].event, LogEvent::AppAdded { id: 1, .. }));
        assert!(matches!(frames[2].event, LogEvent::PacketDelivered { app: 1, packet_id: 42 }));
    }

    #[test]
    fn rejects_missing_header() {
        // Hand-craft a one-frame stream without the header.
        let frame = LogFrame { at_ns: 0, event: LogEvent::AppAdded { id: 1, ip: 0, dst_ip: 0 } };
        let body = postcard::to_allocvec(&frame).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        assert!(matches!(decode_all(&bytes), Err(ReadError::BadHeader)));
    }
}
