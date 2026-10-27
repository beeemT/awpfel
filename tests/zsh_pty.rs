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

// Accept-color probe: each typed key goes through real `zle .self-insert`
// (native ZLE shifts highlight offsets) then production
// `_awpfel_render_ghost` with the fixed full command. Tab / Shift-Tab go
// through production `_awpfel_tab` / `_awpfel_accept_chunk`; the absent
// socket ends the post-accept lookup without async. `__FULL__` is the
// fixed candidate for the test.
const ACCEPT_SCRIPT: &str = r#"
_AWPFEL_PLUGIN_DIR="$AWPFEL_PLUGIN_DIR"
eval "$(sed -e '/^awpfel_init$/d' -e '/^typeset -g _AWPFEL_PLUGIN_DIR=/d' "$AWPFEL_PLUGIN_DIR/awpfel.zsh")"
_AWPFEL_GHOST_STYLE='fg=242'
_AWPFEL_SOCKET="$HOME/absent.sock"
_AWPFEL_FIXED_FULL="__FULL__"
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
_probe_type() {
    zle .self-insert
    _awpfel_render_ghost "$_AWPFEL_FIXED_FULL"
}
_probe_foreign() {
    region_highlight+=("0 3 fg=242 memo=awpfel-extra")
}
zle -N _probe_ready
zle -N _probe_barrier
zle -N _probe_type
zle -N _probe_foreign
zle -N _awpfel_tab
zle -N _awpfel_accept_chunk
for key in {a..z} ' '; do
  bindkey -- "$key" _probe_type
done
bindkey '\t' _awpfel_tab
bindkey '\e[Z' _awpfel_accept_chunk
bindkey '^Xb' _probe_barrier
bindkey '^Xf' _probe_foreign
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
        Self::start_with(SCRIPT)
    }

    fn start_accept(full: &str) -> Option<Self> {
        let script = ACCEPT_SCRIPT.replace("__FULL__", full);
        Self::start_with(&script)
    }

    fn start_with(script_text: &str) -> Option<Self> {
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
        std::fs::write(&script, script_text).unwrap();
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

    fn type_with_ack(&mut self, keys: &[u8]) -> Snapshot {
        let mut snapshot = Snapshot {
            buffer: String::new(),
            cursor: 0,
            row_moves: 0,
        };
        for key in keys {
            snapshot = self.action(std::slice::from_ref(key));
        }
        snapshot
    }

    fn cell_fg(&self, row: u16, col: u16) -> vt100::Color {
        self.parser
            .screen()
            .cell(row, col)
            .unwrap_or_else(|| {
                panic!(
                    "missing cell {row},{col}:\n{}",
                    self.parser.screen().contents()
                )
            })
            .fgcolor()
    }
}

impl Drop for ZleTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_cells(
    terminal: &ZleTerminal,
    row: u16,
    col: u16,
    len: u16,
    expected: vt100::Color,
    what: &str,
) {
    for i in 0..len {
        let actual = terminal.cell_fg(row, col + i);
        assert_eq!(
            actual,
            expected,
            "{what} cell {row},{} must match, got {actual:?}:\n{}",
            col + i,
            terminal.parser.screen().contents()
        );
    }
}

#[test]
fn native_zle_full_accept_repaints_normal_foreground() {
    for typed in ["which", "which "] {
        let Some(mut terminal) = ZleTerminal::start_accept("which omp") else {
            return;
        };
        let row = terminal.parser.screen().cursor_position().0;
        let before = terminal.type_with_ack(typed.as_bytes());
        assert_eq!(before.buffer, typed);
        assert_eq!(before.cursor, typed.len());
        assert!(
            terminal.parser.screen().contents().contains("which omp"),
            "ghost must stage for {typed:?}:\n{}",
            terminal.parser.screen().contents()
        );
        let accepted = terminal.action(b"\t");
        assert_eq!(accepted.buffer, "which omp");
        assert_eq!(accepted.cursor, 9);
        assert_eq!(
            accepted.row_moves, 0,
            "single-line accept must not move below the line"
        );
        assert_cells(
            &terminal,
            row,
            5,
            9,
            vt100::Color::Default,
            "accepted `which omp`",
        );
    }
}

#[test]
fn native_zle_chunk_accept_keeps_remainder_ghost() {
    let Some(mut terminal) = ZleTerminal::start_accept("which omp --version") else {
        return;
    };
    let row = terminal.parser.screen().cursor_position().0;
    let typed = terminal.type_with_ack(b"which ");
    assert_eq!(typed.buffer, "which ");
    assert_eq!(typed.cursor, 6);
    let chunked = terminal.action(b"\x1b[Z");
    assert_eq!(chunked.buffer, "which omp ");
    assert_eq!(chunked.cursor, 10);
    assert_eq!(
        chunked.row_moves, 0,
        "single-line chunk accept must not move below the line"
    );
    assert_cells(
        &terminal,
        row,
        5,
        10,
        vt100::Color::Default,
        "accepted `which omp `",
    );
    assert_cells(
        &terminal,
        row,
        15,
        9,
        vt100::Color::Idx(242),
        "remainder `--version`",
    );
}

#[test]
fn native_zle_accept_preserves_foreign_highlight() {
    let Some(mut terminal) = ZleTerminal::start_accept("which omp") else {
        return;
    };
    let row = terminal.parser.screen().cursor_position().0;
    let typed = terminal.type_with_ack(b"which");
    assert_eq!(typed.buffer, "which");
    assert_eq!(typed.cursor, 5);
    let with_foreign = terminal.action(b"\x18f");
    assert_eq!(with_foreign.buffer, "which");
    assert_eq!(with_foreign.cursor, 5);
    let accepted = terminal.action(b"\t");
    assert_eq!(accepted.buffer, "which omp");
    assert_eq!(accepted.cursor, 9);
    assert_cells(
        &terminal,
        row,
        5,
        3,
        vt100::Color::Idx(242),
        "foreign `whi` range",
    );
    assert_cells(
        &terminal,
        row,
        8,
        6,
        vt100::Color::Default,
        "accepted `ch omp` range",
    );
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
