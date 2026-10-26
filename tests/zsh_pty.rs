#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"
source "$AWPFEL_PLUGIN_DIR/awpfel-render.zsh"
source "$AWPFEL_PLUGIN_DIR/awpfel-communicate.zsh"
_AWPFEL_HINT=''
_AWPFEL_WARNING=''
_AWPFEL_SUGGESTION=''
_AWPFEL_HL_ENTRIES=()
_AWPFEL_SOCKET="$HOME/absent.sock"
KEYTIMEOUT=1
bindkey -e
_probe_ready() {
    zle -R
    builtin printf '0\0\0' >&3
}
_probe_barrier() {
    zle -R
    builtin printf '%s\0%s\0' "$CURSOR" "$BUFFER" >&3
}
_probe_insert() {
    zle .self-insert
    _awpfel_suggest_local
}
_probe_clear() {
    _awpfel_clear_hint
}
_probe_hint() {
    _AWPFEL_HINT='first hint'
    _AWPFEL_WARNING=''
    _awpfel_render_hint
}
_probe_warning() {
    _awpfel_apply_suggest_response '{"suggestions":[],"hint":{"text":"hidden hint","kind":"explanation"},"warning":{"text":"test warning"}}'
}
_probe_empty() {
    _awpfel_apply_suggest_response '{"suggestions":[],"need_ai":false}'
}
zle -N _probe_ready
zle -N _probe_barrier
zle -N _probe_insert
zle -N _probe_clear
zle -N _probe_hint
zle -N _probe_warning
zle -N _probe_empty
bindkey 'g' _probe_insert
bindkey '^Xb' _probe_barrier
bindkey '^Xc' _probe_clear
bindkey '^Xh' _probe_hint
bindkey '^Xw' _probe_warning
bindkey '^Xe' _probe_empty
buffer=''
vared -p 'PTY> ' -i _probe_ready buffer
"#;

struct Snapshot {
    buffer: String,
    cursor: usize,
    row_moves: usize,
}

struct ZleTerminal {
    child: Child,
    master: File,
    control: UnixStream,
    parser: vt100::Parser<()>,
    row_moves: usize,
    _home: tempfile::TempDir,
}

impl ZleTerminal {
    fn start() -> Option<Self> {
        for program in ["zsh", "jq"] {
            match Command::new(program).arg("--version").output() {
                Ok(output) => assert!(output.status.success(), "{program} is not usable"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    eprintln!("skipping native ZLE test: {program} is not installed");
                    return None;
                }
                Err(error) => panic!("cannot run {program}: {error}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("probe.zsh");
        std::fs::write(&script, SCRIPT).unwrap();
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors; all other pointers are
        // null or point to a valid winsize for the duration of the call.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut::<libc::termios>(),
                &mut size,
            )
        };
        assert_eq!(result, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: openpty returned two distinct, newly owned descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let (control, child_control) = UnixStream::pair().unwrap();
        let control_fd = child_control.as_raw_fd();
        let mut command = Command::new("zsh");
        command
            .arg("-dfi")
            .arg(script)
            .env("HOME", home.path())
            .env("ZDOTDIR", home.path())
            .env("TERM", "xterm-256color")
            .env(
                "AWPFEL_PLUGIN_DIR",
                concat!(env!("CARGO_MANIFEST_DIR"), "/plugin"),
            )
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: the child only calls async-signal-safe libc functions before
        // exec. Descriptor 0 is the PTY slave installed by Command.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0
                    || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0
                    || libc::dup2(control_fd, 3) < 0
                    || libc::fcntl(3, libc::F_SETFD, 0) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(child_control);
        // SAFETY: master is a live descriptor owned above. Preserve its other
        // flags while enabling nonblocking reads for the poll-driven barrier.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
        }
        control.set_nonblocking(true).unwrap();
        let mut terminal = Self {
            child,
            master,
            control,
            parser: vt100::Parser::new(
                NonZeroU16::new(24).unwrap(),
                NonZeroU16::new(80).unwrap(),
                0,
            ),
            row_moves: 0,
            _home: home,
        };
        terminal.wait_snapshot();
        terminal.action(b"");
        Some(terminal)
    }

    fn drain_terminal(&mut self) {
        let mut bytes = [0; 4096];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => panic!("Zsh exited: {}", self.parser.screen().contents()),
                Ok(count) => {
                    for byte in &bytes[..count] {
                        let row = self.parser.screen().cursor_position().0;
                        self.parser.process(std::slice::from_ref(byte));
                        if row != self.parser.screen().cursor_position().0
                            && !self.parser.screen().hide_cursor()
                        {
                            self.row_moves += 1;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("PTY read: {error}: {}", self.parser.screen().contents()),
            }
        }
    }

    fn wait_snapshot(&mut self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut response = Vec::new();
        let mut separators = 0;
        while separators < 2 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "ZLE barrier timed out: {}",
                self.parser.screen().contents()
            );
            let mut fds = [
                libc::pollfd {
                    fd: self.master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.control.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: fds is a valid two-element pollfd array for this call.
            let ready = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    remaining.as_millis().max(1) as i32,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("poll: {error}");
            }
            self.drain_terminal();
            let mut bytes = [0; 256];
            match self.control.read(&mut bytes) {
                Ok(0) => panic!("ZLE control closed: {}", self.parser.screen().contents()),
                Ok(count) => {
                    separators += bytes[..count].iter().filter(|byte| **byte == 0).count();
                    response.extend_from_slice(&bytes[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("ZLE control read: {error}"),
            }
        }
        // The barrier forces redisplay before writing its control response.
        // Drain bytes already emitted before it, including transient moves.
        self.drain_terminal();
        let mut fields = response.split(|byte| *byte == 0);
        let cursor = std::str::from_utf8(fields.next().unwrap())
            .unwrap()
            .parse()
            .unwrap();
        let buffer = String::from_utf8(fields.next().unwrap().to_vec()).unwrap();
        Snapshot {
            buffer,
            cursor,
            row_moves: self.row_moves,
        }
    }

    fn action(&mut self, keys: &[u8]) -> Snapshot {
        self.row_moves = 0;
        self.master.write_all(keys).unwrap();
        self.master.write_all(b"\x18b").unwrap();
        self.wait_snapshot()
    }
}

impl Drop for ZleTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn native_zle_message_transitions_preserve_cursor_and_remove_stale_text() {
    let Some(mut terminal) = ZleTerminal::start() else {
        return;
    };
    let row = terminal.parser.screen().cursor_position().0;
    let typed = terminal.action(b"g");
    assert_eq!(typed.buffer, "g");
    assert_eq!(typed.cursor, 1);
    assert_eq!(
        typed.row_moves, 0,
        "no-socket typing must not move below the line"
    );

    let clear = terminal.action(b"\x18c");
    assert_eq!(
        clear.row_moves, 0,
        "clearing an absent message must be silent"
    );
    assert_eq!(clear.buffer, "g");
    assert_eq!(clear.cursor, 1);

    terminal.action(b"\x18h");
    assert!(terminal.parser.screen().contents().contains("first hint"));
    let repeated = terminal.action(b"\x18h");
    assert_eq!(repeated.row_moves, 0, "an unchanged hint must not repaint");
    assert!(terminal.parser.screen().contents().contains("first hint"));

    terminal.action(b"\x18w");
    let warning_screen = terminal.parser.screen().contents();
    assert!(warning_screen.contains("test warning"));
    assert!(
        !warning_screen.contains("first hint"),
        "old hint remains at {:?}:\n{warning_screen}",
        terminal.parser.screen().cursor_position()
    );
    assert!(!warning_screen.contains("hidden hint"));

    terminal.action(b"\x18e");
    let empty_screen = terminal.parser.screen().contents();
    assert!(!empty_screen.contains("test warning"));
    assert!(!empty_screen.contains("hidden hint"));
    assert_eq!(terminal.action(b"\x18e").row_moves, 0);

    terminal.action(b"\x18h");
    let cleared = terminal.action(b"\x18c");
    assert!(!terminal.parser.screen().contents().contains("first hint"));
    assert_eq!(cleared.buffer, "g");
    assert_eq!(cleared.cursor, 1);
    assert_eq!(terminal.parser.screen().cursor_position().0, row);
}
