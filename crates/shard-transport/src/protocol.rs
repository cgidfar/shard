use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum accepted session frame size, including the type byte.
pub const MAX_SESSION_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Activity state pushed by harness hooks to the supervisor, then fanned out
/// to all connected clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActivityState {
    /// Agent is actively working (processing prompt, executing tools).
    Active = 0x00,
    /// Agent is idle (finished responding, waiting for user input).
    Idle = 0x01,
    /// Agent is blocked (waiting for permission approval).
    Blocked = 0x02,
}

impl TryFrom<u8> for ActivityState {
    type Error = u8;
    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Active),
            0x01 => Ok(Self::Idle),
            0x02 => Ok(Self::Blocked),
            other => Err(other),
        }
    }
}

/// Frame types for the session transport protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Terminal output from PTY (supervisor -> client).
    /// Contains byte offset for resume support.
    TerminalOutput { offset: u64, data: Vec<u8> },

    /// Resize request (client -> supervisor).
    Resize { rows: u16, cols: u16 },

    /// Terminal input (client -> supervisor), forwarded to PTY stdin.
    TerminalInput { data: Vec<u8> },

    /// Graceful stop request (client -> supervisor).
    StopGraceful,

    /// Force stop request (client -> supervisor).
    StopForce,

    /// Lifecycle status (supervisor -> client).
    /// 0 = exited normally, 1 = stopped, 2 = failed.
    /// Sent once on session termination.
    Status { code: u8 },

    /// Resume request (client -> supervisor).
    /// Client sends its last seen offset to resume from.
    /// A value of `u64::MAX` is a sentinel meaning "skip replay, live-only".
    Resume { last_seen_offset: u64 },

    /// Activity state change (supervisor -> client).
    /// Pushed on transitions (idle ↔ active), not periodically.
    ActivityUpdate { state: ActivityState },
}

const TYPE_TERMINAL_OUTPUT: u8 = 0x00;
const TYPE_RESIZE: u8 = 0x01;
const TYPE_TERMINAL_INPUT: u8 = 0x02;
const TYPE_STOP_GRACEFUL: u8 = 0x03;
const TYPE_STOP_FORCE: u8 = 0x04;
const TYPE_STATUS: u8 = 0x05;
const TYPE_RESUME: u8 = 0x06;
const TYPE_ACTIVITY_UPDATE: u8 = 0x07;

/// Write a frame to an async writer.
///
/// Wire format: [u32 length][u8 type][payload]
/// Length includes the type byte + payload.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> std::io::Result<()> {
    let mut payload = Vec::new();

    let type_byte = match frame {
        Frame::TerminalOutput { offset, data } => {
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(data);
            TYPE_TERMINAL_OUTPUT
        }
        Frame::Resize { rows, cols } => {
            payload.extend_from_slice(&rows.to_be_bytes());
            payload.extend_from_slice(&cols.to_be_bytes());
            TYPE_RESIZE
        }
        Frame::TerminalInput { data } => {
            payload.extend_from_slice(data);
            TYPE_TERMINAL_INPUT
        }
        Frame::StopGraceful => TYPE_STOP_GRACEFUL,
        Frame::StopForce => TYPE_STOP_FORCE,
        Frame::Status { code } => {
            payload.push(*code);
            TYPE_STATUS
        }
        Frame::Resume { last_seen_offset } => {
            payload.extend_from_slice(&last_seen_offset.to_be_bytes());
            TYPE_RESUME
        }
        Frame::ActivityUpdate { state } => {
            payload.push(*state as u8);
            TYPE_ACTIVITY_UPDATE
        }
    };

    let length = 1 + payload.len() as u32; // type byte + payload
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&[type_byte]).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a frame from an async reader.
///
/// Returns None on clean EOF (reader closed).
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Frame>> {
    let Some(length) = read_frame_len(reader, MAX_SESSION_FRAME_LEN, "session").await? else {
        return Ok(None);
    };

    if length == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame length is zero",
        ));
    }

    // Read type + payload
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).await?;

    let type_byte = buf[0];
    let payload = &buf[1..];

    let frame = match type_byte {
        TYPE_TERMINAL_OUTPUT => {
            if payload.len() < 8 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "terminal output frame too short",
                ));
            }
            let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
            let data = payload[8..].to_vec();
            Frame::TerminalOutput { offset, data }
        }
        TYPE_RESIZE => {
            if payload.len() != 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "resize frame must be exactly 4 bytes",
                ));
            }
            let rows = u16::from_be_bytes(payload[..2].try_into().unwrap());
            let cols = u16::from_be_bytes(payload[2..4].try_into().unwrap());
            Frame::Resize { rows, cols }
        }
        TYPE_TERMINAL_INPUT => Frame::TerminalInput {
            data: payload.to_vec(),
        },
        TYPE_STOP_GRACEFUL => {
            if !payload.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stop graceful frame has trailing bytes",
                ));
            }
            Frame::StopGraceful
        }
        TYPE_STOP_FORCE => {
            if !payload.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stop force frame has trailing bytes",
                ));
            }
            Frame::StopForce
        }
        TYPE_STATUS => {
            if payload.len() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "status frame must be exactly 1 byte",
                ));
            }
            Frame::Status { code: payload[0] }
        }
        TYPE_RESUME => {
            if payload.len() != 8 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "resume frame must be exactly 8 bytes",
                ));
            }
            let last_seen_offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
            Frame::Resume { last_seen_offset }
        }
        TYPE_ACTIVITY_UPDATE => {
            if payload.len() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "activity update frame must be exactly 1 byte",
                ));
            }
            let state = ActivityState::try_from(payload[0]).map_err(|b| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown activity state: 0x{b:02x}"),
                )
            })?;
            Frame::ActivityUpdate { state }
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown frame type: 0x{type_byte:02x}"),
            ));
        }
    };

    Ok(Some(frame))
}

async fn read_frame_len<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
    label: &str,
) -> std::io::Result<Option<usize>> {
    let mut len_buf = [0u8; 4];
    let mut read = 0;
    while read < len_buf.len() {
        let n = reader.read(&mut len_buf[read..]).await?;
        if n == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{label} frame length prefix ended after {read} bytes"),
            ));
        }
        read += n;
    }

    let length = usize::try_from(u32::from_be_bytes(len_buf)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} frame length does not fit usize"),
        )
    })?;
    if length > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} frame length {length} exceeds max {max_len}"),
        ));
    }

    Ok(Some(length))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(frame: Frame) -> Frame {
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        read_frame(&mut cursor).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn roundtrip_terminal_output() {
        let frame = Frame::TerminalOutput {
            offset: 42,
            data: b"hello world".to_vec(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_resize() {
        let frame = Frame::Resize { rows: 24, cols: 80 };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_terminal_input() {
        let frame = Frame::TerminalInput {
            data: b"ls\r\n".to_vec(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_stop_graceful() {
        assert_eq!(roundtrip(Frame::StopGraceful).await, Frame::StopGraceful);
    }

    #[tokio::test]
    async fn roundtrip_stop_force() {
        assert_eq!(roundtrip(Frame::StopForce).await, Frame::StopForce);
    }

    #[tokio::test]
    async fn roundtrip_status() {
        let frame = Frame::Status { code: 0 };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_resume() {
        let frame = Frame::Resume {
            last_seen_offset: 1024,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_activity_update_active() {
        let frame = Frame::ActivityUpdate {
            state: ActivityState::Active,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_activity_update_idle() {
        let frame = Frame::ActivityUpdate {
            state: ActivityState::Idle,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_activity_update_blocked() {
        let frame = Frame::ActivityUpdate {
            state: ActivityState::Blocked,
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn read_eof_returns_none() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_frame_before_allocation() {
        let mut cursor =
            std::io::Cursor::new(((MAX_SESSION_FRAME_LEN as u32) + 1).to_be_bytes().to_vec());
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn partial_length_prefix_is_invalid_data() {
        let mut cursor = std::io::Cursor::new(vec![0, 0]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_unknown_type() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 1, 0xff]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_fixed_frame_trailing_bytes() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 2, TYPE_STOP_GRACEFUL, 0]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_invalid_activity_state() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 2, TYPE_ACTIVITY_UPDATE, 0xff]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
