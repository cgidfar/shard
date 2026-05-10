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

/// Identifies the kind of client connecting to a session supervisor. Used by
/// the supervisor to make ownership policy decisions and surfaced in
/// `InputOwnerChanged` so observers can describe the current owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ClientKind {
    /// A Tauri app window. `label` carries the window label.
    GuiWindow = 0x00,
    /// A CLI process (e.g. `shardctl attach`).
    CliAgent = 0x01,
    /// Read-only observer; never claims input. Replaces the historical
    /// `Resume { last_seen_offset: u64::MAX }` sentinel for new clients.
    MonitorOnly = 0x02,
}

impl TryFrom<u8> for ClientKind {
    type Error = u8;
    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::GuiWindow),
            0x01 => Ok(Self::CliAgent),
            0x02 => Ok(Self::MonitorOnly),
            other => Err(other),
        }
    }
}

/// Snapshot of who currently owns PTY input for a session, broadcast inside
/// `InputOwnerChanged`. `client_id` and `label` are the values the owner
/// supplied in its `Hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSummary {
    pub client_id: u64,
    pub kind: ClientKind,
    pub label: String,
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

    /// Client identification (client -> supervisor). Required as the first
    /// frame on every streaming connection. The harness-hook fire-and-forget
    /// path sends `ActivityUpdate` as its only frame and is exempt.
    Hello {
        client_id: u64,
        kind: ClientKind,
        label: String,
    },

    /// Request input ownership of the PTY (client -> supervisor). The
    /// supervisor identifies the claimer by the `client_id` from the
    /// preceding `Hello`. GUI clients may not displace a different GUI
    /// owner; CLI clients may temporarily take over. Ownership is released
    /// on disconnect of the owner. `MonitorOnly` clients must not send this.
    ClaimInput,

    /// Claim rejection (supervisor -> client). Sent when a `ClaimInput`
    /// violates the supervisor's ownership policy. `owner` is the current
    /// owner, when known, so GUI callers can focus the owning window.
    ClaimRejected { owner: Option<OwnerSummary> },

    /// Input-owner snapshot/transition (supervisor -> client). Broadcast on
    /// every ownership change and sent once to each new client right after
    /// replay so late joiners learn the current state.
    InputOwnerChanged { owner: Option<OwnerSummary> },
}

const TYPE_TERMINAL_OUTPUT: u8 = 0x00;
const TYPE_RESIZE: u8 = 0x01;
const TYPE_TERMINAL_INPUT: u8 = 0x02;
const TYPE_STOP_GRACEFUL: u8 = 0x03;
const TYPE_STOP_FORCE: u8 = 0x04;
const TYPE_STATUS: u8 = 0x05;
const TYPE_RESUME: u8 = 0x06;
const TYPE_ACTIVITY_UPDATE: u8 = 0x07;
const TYPE_HELLO: u8 = 0x08;
const TYPE_CLAIM_INPUT: u8 = 0x09;
const TYPE_INPUT_OWNER_CHANGED: u8 = 0x0A;
const TYPE_CLAIM_REJECTED: u8 = 0x0B;

/// Append a `[u16 len][bytes]` UTF-8 string to a frame payload, with a
/// uniform error message tag for the length-overflow case. Used for both
/// `Hello.label` and `InputOwnerChanged.owner.label`.
fn push_length_prefixed_label(
    payload: &mut Vec<u8>,
    label: &str,
    context: &str,
) -> std::io::Result<()> {
    let bytes = label.as_bytes();
    let len = u16::try_from(bytes.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{context} label too long: {} bytes", bytes.len()),
        )
    })?;
    payload.extend_from_slice(&len.to_be_bytes());
    payload.extend_from_slice(bytes);
    Ok(())
}

fn push_optional_owner(payload: &mut Vec<u8>, owner: &Option<OwnerSummary>) -> std::io::Result<()> {
    match owner {
        None => payload.push(0x00),
        Some(o) => {
            payload.push(0x01);
            payload.extend_from_slice(&o.client_id.to_be_bytes());
            payload.push(o.kind as u8);
            push_length_prefixed_label(payload, &o.label, "owner")?;
        }
    }
    Ok(())
}

fn read_optional_owner(payload: &[u8], frame_name: &str) -> std::io::Result<Option<OwnerSummary>> {
    if payload.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{frame_name} frame must have a tag byte"),
        ));
    }
    match payload[0] {
        0x00 => {
            if payload.len() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{frame_name} None frame has trailing bytes"),
                ));
            }
            Ok(None)
        }
        0x01 => {
            if payload.len() < 12 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{frame_name} Some frame too short"),
                ));
            }
            let client_id = u64::from_be_bytes(payload[1..9].try_into().unwrap());
            let kind = ClientKind::try_from(payload[9]).map_err(|b| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown client kind in owner summary: 0x{b:02x}"),
                )
            })?;
            let label_len = u16::from_be_bytes(payload[10..12].try_into().unwrap()) as usize;
            if payload.len() != 12 + label_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "owner summary label length does not match payload",
                ));
            }
            let label = std::str::from_utf8(&payload[12..12 + label_len])
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("owner summary label not utf-8: {e}"),
                    )
                })?
                .to_string();
            Ok(Some(OwnerSummary {
                client_id,
                kind,
                label,
            }))
        }
        tag => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown owner tag byte: 0x{tag:02x}"),
        )),
    }
}

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
        Frame::Hello {
            client_id,
            kind,
            label,
        } => {
            payload.extend_from_slice(&client_id.to_be_bytes());
            payload.push(*kind as u8);
            push_length_prefixed_label(&mut payload, label, "hello")?;
            TYPE_HELLO
        }
        Frame::ClaimInput => TYPE_CLAIM_INPUT,
        Frame::ClaimRejected { owner } => {
            push_optional_owner(&mut payload, owner)?;
            TYPE_CLAIM_REJECTED
        }
        Frame::InputOwnerChanged { owner } => {
            push_optional_owner(&mut payload, owner)?;
            TYPE_INPUT_OWNER_CHANGED
        }
    };

    let length = u32::try_from(1 + payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {} bytes", payload.len()),
        )
    })?;
    if (length as usize) > MAX_SESSION_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds MAX_SESSION_FRAME_LEN: {length}"),
        ));
    }
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
            let code = match payload.len() {
                1 => payload[0],
                4 => {
                    let legacy = u32::from_be_bytes(payload.try_into().unwrap());
                    u8::try_from(legacy).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("legacy status code {legacy} exceeds u8"),
                        )
                    })?
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "status frame must be 1 byte or legacy 4-byte code",
                    ));
                }
            };
            Frame::Status { code }
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
        TYPE_HELLO => {
            if payload.len() < 11 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "hello frame too short",
                ));
            }
            let client_id = u64::from_be_bytes(payload[..8].try_into().unwrap());
            let kind = ClientKind::try_from(payload[8]).map_err(|b| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown client kind: 0x{b:02x}"),
                )
            })?;
            let label_len = u16::from_be_bytes(payload[9..11].try_into().unwrap()) as usize;
            if payload.len() != 11 + label_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "hello label length does not match payload",
                ));
            }
            let label = std::str::from_utf8(&payload[11..11 + label_len])
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("hello label not utf-8: {e}"),
                    )
                })?
                .to_string();
            Frame::Hello {
                client_id,
                kind,
                label,
            }
        }
        TYPE_CLAIM_INPUT => {
            if !payload.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "claim input frame has trailing bytes",
                ));
            }
            Frame::ClaimInput
        }
        TYPE_CLAIM_REJECTED => Frame::ClaimRejected {
            owner: read_optional_owner(payload, "claim rejected")?,
        },
        TYPE_INPUT_OWNER_CHANGED => Frame::InputOwnerChanged {
            owner: read_optional_owner(payload, "input owner changed")?,
        },
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
    async fn accepts_legacy_four_byte_status_payload() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 5, TYPE_STATUS, 0, 0, 0, 0]);
        assert_eq!(
            read_frame(&mut cursor).await.unwrap(),
            Some(Frame::Status { code: 0 })
        );
    }

    #[tokio::test]
    async fn parses_legacy_four_byte_status_payload_as_u32() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 5, TYPE_STATUS, 0, 0, 0, 1]);
        assert_eq!(
            read_frame(&mut cursor).await.unwrap(),
            Some(Frame::Status { code: 1 })
        );
    }

    #[tokio::test]
    async fn rejects_legacy_status_payload_outside_u8_range() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 5, TYPE_STATUS, 0, 0, 1, 0]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_status_payload_with_unrecognized_trailing_bytes() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 3, TYPE_STATUS, 0, 0]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_invalid_activity_state() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 2, TYPE_ACTIVITY_UPDATE, 0xff]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn roundtrip_hello_gui() {
        let frame = Frame::Hello {
            client_id: 0xDEAD_BEEF_CAFE_F00D,
            kind: ClientKind::GuiWindow,
            label: "main".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_hello_cli_empty_label() {
        let frame = Frame::Hello {
            client_id: 1,
            kind: ClientKind::CliAgent,
            label: String::new(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_hello_monitor() {
        let frame = Frame::Hello {
            client_id: 42,
            kind: ClientKind::MonitorOnly,
            label: "tauri:window-2".to_string(),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_claim_input() {
        assert_eq!(roundtrip(Frame::ClaimInput).await, Frame::ClaimInput);
    }

    #[tokio::test]
    async fn roundtrip_claim_rejected_with_owner() {
        let frame = Frame::ClaimRejected {
            owner: Some(OwnerSummary {
                client_id: 7,
                kind: ClientKind::GuiWindow,
                label: "window-7".to_string(),
            }),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_claim_rejected_without_owner() {
        let frame = Frame::ClaimRejected { owner: None };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_input_owner_changed_none() {
        let frame = Frame::InputOwnerChanged { owner: None };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn roundtrip_input_owner_changed_some() {
        let frame = Frame::InputOwnerChanged {
            owner: Some(OwnerSummary {
                client_id: 0x1234_5678_9ABC_DEF0,
                kind: ClientKind::GuiWindow,
                label: "main".to_string(),
            }),
        };
        assert_eq!(roundtrip(frame.clone()).await, frame);
    }

    #[tokio::test]
    async fn rejects_unknown_client_kind() {
        let mut cursor = std::io::Cursor::new(vec![
            0, 0, 0, 12, TYPE_HELLO, 0, 0, 0, 0, 0, 0, 0, 1, 0xEE, 0, 0,
        ]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_claim_input_with_payload() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 2, TYPE_CLAIM_INPUT, 0xAA]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_input_owner_unknown_tag() {
        let mut cursor = std::io::Cursor::new(vec![0, 0, 0, 2, TYPE_INPUT_OWNER_CHANGED, 0xAA]);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
