use tokio::io::AsyncWriteExt;

use shard_transport::protocol::{self, Frame};
use shard_transport::transport_windows::NamedPipeTransport;
use shard_transport::SessionTransport;

/// Attach to a session via its transport address.
///
/// Bridges the local terminal to the remote PTY:
/// - Local stdin → TerminalInput frames → supervisor → PTY
/// - PTY → TerminalOutput frames → supervisor → local stdout
///
/// Detach with Ctrl-] (0x1d).
pub async fn attach_to_session(transport_addr: &str) -> shard_core::Result<()> {
    let client = NamedPipeTransport::connect(transport_addr)
        .await
        .map_err(|e| shard_core::ShardError::Other(format!("connect failed: {e}")))?;

    // Enable raw mode for the local terminal
    crossterm::terminal::enable_raw_mode()
        .map_err(|e| shard_core::ShardError::Other(format!("raw mode failed: {e}")))?;

    let result = run_attach(client).await;

    // Always restore terminal
    let _ = crossterm::terminal::disable_raw_mode();

    result
}

async fn run_attach(
    client: tokio::net::windows::named_pipe::NamedPipeClient,
) -> shard_core::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(client);

    // Send resume frame with offset 0 (fresh attach)
    protocol::write_frame(
        &mut writer,
        &Frame::Resume {
            last_seen_offset: 0,
        },
    )
    .await
    .map_err(shard_core::ShardError::Io)?;

    // === Stdin → pipe ===
    let stdin_task = tokio::spawn(async move {
        use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

        loop {
            // crossterm event reading
            let evt = tokio::task::spawn_blocking(event::read).await;

            match evt {
                Ok(Ok(Event::Key(KeyEvent {
                    code: KeyCode::Char(']'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }))) => {
                    // Detach
                    break;
                }
                Ok(Ok(Event::Key(key_event))) => {
                    let bytes = key_event_to_bytes(key_event);
                    if !bytes.is_empty() {
                        let frame = Frame::TerminalInput { data: bytes };
                        if protocol::write_frame(&mut writer, &frame).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(Ok(Event::Paste(text))) => {
                    let frame = Frame::TerminalInput {
                        data: text.into_bytes(),
                    };
                    if protocol::write_frame(&mut writer, &frame).await.is_err() {
                        break;
                    }
                }
                Ok(Ok(_)) => {} // Ignore mouse, resize, etc.
                Ok(Err(_)) | Err(_) => break,
            }
        }
    });

    // === Pipe → stdout ===
    let stdout_task = tokio::spawn(async move {
        let mut stdout: tokio::io::Stdout = tokio::io::stdout();
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(Frame::TerminalOutput { data, .. })) => {
                    if stdout.write_all(&data).await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                }
                Ok(Some(Frame::Status { code })) => {
                    tracing::debug!("session status: {code}");
                    break;
                }
                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
    });

    tokio::select! {
        _ = stdin_task => {}
        _ = stdout_task => {}
    }

    println!("\r\n[detached]");

    Ok(())
}

/// Convert a crossterm KeyEvent to raw bytes for the PTY.
fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};

    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, etc.
                let ctrl = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                if ctrl <= 26 {
                    return vec![ctrl];
                }
            }
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}
