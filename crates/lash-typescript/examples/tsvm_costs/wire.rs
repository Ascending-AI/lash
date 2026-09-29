use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

pub const FRAME_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub sequence: u64,
    pub operation: String,
    pub payload: String,
}

pub fn write_frame(writer: &mut impl Write, message: &Message) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds benchmark cap",
        ));
    }
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Message> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds benchmark cap",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub struct Worker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl Worker {
    pub fn spawn() -> io::Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--worker")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut worker = Self {
            child,
            input,
            output,
        };
        let ready = read_frame(&mut worker.output)?;
        if ready.operation != "ready" || ready.sequence != 0 {
            return Err(io::Error::other("invalid ready message"));
        }
        Ok(worker)
    }

    pub fn send(&mut self, message: &Message) -> io::Result<()> {
        write_frame(&mut self.input, message)
    }

    pub fn receive(&mut self) -> io::Result<Message> {
        read_frame(&mut self.output)
    }

    pub fn exchange(&mut self, message: &Message) -> io::Result<Message> {
        self.send(message)?;
        let reply = self.receive()?;
        if reply.sequence != message.sequence {
            return Err(io::Error::other("sequence mismatch"));
        }
        Ok(reply)
    }

    pub fn rss_kib(&self) -> io::Result<u64> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id()))?;
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .ok_or_else(|| io::Error::other("VmRSS unavailable"))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn verify() -> io::Result<()> {
    use std::io::Cursor;
    let message = Message {
        sequence: 7,
        operation: "echo".into(),
        payload: "\"\nλ".into(),
    };
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &message)?;
    assert_eq!(read_frame(&mut Cursor::new(&bytes))?, message);
    for length in 0..bytes.len() {
        assert!(read_frame(&mut Cursor::new(&bytes[..length])).is_err());
    }
    let oversized = u32::try_from(FRAME_LIMIT + 1)
        .map_err(io::Error::other)?
        .to_be_bytes();
    assert_eq!(
        read_frame(&mut Cursor::new(oversized))
            .expect_err("oversized frame")
            .kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}
