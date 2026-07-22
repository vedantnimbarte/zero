//! The pipe between the browser process and a renderer.
//!
//! Length-prefixed frames of tagged fields, written by hand. It is a few
//! hundred bytes of code against a serialization crate's dependency tree, and
//! the whole format is visible on one screen — which matters more than usual
//! for the one surface that reads bytes a website influenced.
//!
//! Every read is bounded: a length that would allocate more than [`MAX_FRAME`]
//! is refused rather than trusted, because the far end of this pipe is the
//! process handling hostile input.

use std::io::{Read, Write};

/// The largest frame either side will accept. A 4K window's pixels are ~33 MB,
/// so the ceiling has to clear that and nothing more.
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// One message: a name, some strings, some numbers, and at most one blob.
///
/// Deliberately not a typed enum per message — the two ends agree on the name
/// and what follows it, and a mismatch is a protocol error either way.
#[derive(Debug, Default, PartialEq)]
pub struct Msg {
    pub name: String,
    pub text: Vec<String>,
    pub nums: Vec<f64>,
    pub blob: Vec<u8>,
}

impl Msg {
    pub fn new(name: &str) -> Msg {
        Msg { name: name.to_string(), ..Default::default() }
    }

    pub fn text(mut self, value: impl Into<String>) -> Msg {
        self.text.push(value.into());
        self
    }

    pub fn num(mut self, value: f64) -> Msg {
        self.nums.push(value);
        self
    }

    pub fn blob(mut self, bytes: Vec<u8>) -> Msg {
        self.blob = bytes;
        self
    }

    /// The `i`th string, or empty — a missing field is not worth a panic in a
    /// process whose peer may be misbehaving.
    pub fn str_at(&self, i: usize) -> &str {
        self.text.get(i).map(String::as_str).unwrap_or("")
    }

    pub fn num_at(&self, i: usize) -> f64 {
        self.nums.get(i).copied().unwrap_or(0.0)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put_str = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        put_str(&mut out, &self.name);
        out.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        for s in &self.text {
            put_str(&mut out, s);
        }
        out.extend_from_slice(&(self.nums.len() as u32).to_le_bytes());
        for n in &self.nums {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&(self.blob.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.blob);
        // The frame length goes in front, so the reader knows what to expect.
        let mut framed = (out.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&out);
        framed
    }
}

pub fn write(to: &mut impl Write, msg: &Msg) -> std::io::Result<()> {
    to.write_all(&msg.encode())?;
    to.flush()
}

/// Read one message. `None` at a clean end of stream; an error for anything
/// malformed, which the caller treats as the peer being gone.
pub fn read(from: &mut impl Read) -> std::io::Result<Option<Msg>> {
    let Some(len) = read_u32(from)? else { return Ok(None) };
    let mut body = vec![0u8; bounded(len)?];
    from.read_exact(&mut body)?;
    let mut cursor = std::io::Cursor::new(body);

    let name = read_string(&mut cursor)?;
    let mut text = Vec::new();
    for _ in 0..count(&mut cursor)? {
        text.push(read_string(&mut cursor)?);
    }
    let mut nums = Vec::new();
    for _ in 0..count(&mut cursor)? {
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf)?;
        nums.push(f64::from_le_bytes(buf));
    }
    let blob_len = count(&mut cursor)?;
    let mut blob = vec![0u8; bounded(blob_len as u32)?];
    cursor.read_exact(&mut blob)?;
    Ok(Some(Msg { name, text, nums, blob }))
}

fn bounded(len: u32) -> std::io::Result<usize> {
    match len as usize <= MAX_FRAME {
        true => Ok(len as usize),
        false => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame larger than the ceiling",
        )),
    }
}

fn read_u32(from: &mut impl Read) -> std::io::Result<Option<u32>> {
    let mut buf = [0u8; 4];
    match from.read_exact(&mut buf) {
        Ok(()) => Ok(Some(u32::from_le_bytes(buf))),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

fn count(from: &mut impl Read) -> std::io::Result<usize> {
    let mut buf = [0u8; 4];
    from.read_exact(&mut buf)?;
    // A count is bounded by the frame it came from, which is already capped.
    Ok(u32::from_le_bytes(buf) as usize)
}

fn read_string(from: &mut impl Read) -> std::io::Result<String> {
    let mut len = [0u8; 4];
    from.read_exact(&mut len)?;
    let mut bytes = vec![0u8; bounded(u32::from_le_bytes(len))?];
    from.read_exact(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_survives_the_pipe() {
        let msg = Msg::new("render")
            .text("https://example.org")
            .text("")
            .num(1280.0)
            .num(800.5)
            .blob(vec![1, 2, 3, 250]);
        let mut pipe = std::io::Cursor::new(msg.encode());
        assert_eq!(read(&mut pipe).expect("read").as_ref(), Some(&msg));
        // Nothing follows: a clean end, not an error.
        assert_eq!(read(&mut pipe).expect("read"), None);
    }

    #[test]
    fn several_messages_queue_up_in_order() {
        let mut pipe = Vec::new();
        for i in 0..3 {
            write(&mut pipe, &Msg::new("frame").num(i as f64)).expect("write");
        }
        let mut pipe = std::io::Cursor::new(pipe);
        for i in 0..3 {
            let msg = read(&mut pipe).expect("read").expect("a message");
            assert_eq!(msg.num_at(0), i as f64);
        }
        assert_eq!(read(&mut pipe).expect("read"), None);
    }

    #[test]
    fn an_absurd_length_is_refused_rather_than_allocated() {
        // The far end of this pipe handles hostile input; a 3 GB "string" is
        // an attack on this process's memory, not a message.
        let mut framed = u32::MAX.to_le_bytes().to_vec();
        framed.extend_from_slice(b"junk");
        let mut pipe = std::io::Cursor::new(framed);
        assert!(read(&mut pipe).is_err());
    }

    #[test]
    fn missing_fields_read_as_empty_rather_than_panicking() {
        let msg = Msg::new("click");
        assert_eq!(msg.str_at(3), "");
        assert_eq!(msg.num_at(0), 0.0);
    }
}
