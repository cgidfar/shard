use std::collections::HashMap;

use tokio::io::WriteHalf;
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::Mutex;

pub struct SessionWriter {
    pub writer: WriteHalf<NamedPipeClient>,
}

pub struct AppState {
    pub sessions: Mutex<HashMap<String, SessionWriter>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}
