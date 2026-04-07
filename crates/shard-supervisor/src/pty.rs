use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;

/// A spawned PTY session with handles for I/O.
pub struct PtySession {
    pub child: Box<dyn Child + Send>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub master: Box<dyn MasterPty + Send>,
}

impl PtySession {
    /// Spawn a command in a new PTY.
    ///
    /// `envs` is a list of extra environment variables injected into the child.
    /// The child also inherits the supervisor's full environment.
    pub fn spawn(
        command: &[String],
        working_dir: &Path,
        rows: u16,
        cols: u16,
        envs: &[(&str, &str)],
    ) -> std::io::Result<Self> {
        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut cmd = CommandBuilder::new(&command[0]);
        if command.len() > 1 {
            cmd.args(&command[1..]);
        }
        cmd.cwd(working_dir);
        for (key, val) in envs {
            cmd.env(key, val);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        // Drop the slave — the child now owns it
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        Ok(PtySession {
            child,
            reader,
            writer,
            master: pair.master,
        })
    }

    /// Resize the PTY.
    pub fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    }

    /// Get the child process ID, if available.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}
