//! Compositor control socket behind `shoji_wm --quit` / `--reload` /
//! `--frame-stats` / `--frame-debug` / `--perf-option`.
//!
//! The same two escape hatches as `Super+Shift+Q` / `Super+Shift+R`, reachable
//! from a terminal, a status bar's click handler or a config keybinding that
//! spawns the command. It belongs to the compositor rather than the config
//! runtime on purpose: reloading is how a broken config gets fixed, so it has
//! to work while the config is the thing that is failing.
//!
//! The socket is `$XDG_RUNTIME_DIR/shojiwm.<WAYLAND_DISPLAY>.sock`, exported as
//! `SHOJIWM_SOCKET`. The wire format is one command per connection:
//!
//! ```text
//! client -> server   "quit\n" | "reload\n" | "frame-stats\n"
//!                    | "frame-debug on\n" | "frame-debug off\n" | "frame-debug\n"
//!                    | "perf-option NAME on\n" | "perf-option NAME off\n" | "perf-options\n"
//! server -> client   "ok\n" | "<one line of JSON>\n" | "error: <message>\n"
//! ```
//!
//! `frame-stats` and `frame-debug` are answered from this socket's thread
//! ([`crate::frame_stats`]), never the event loop's: the per-output counters,
//! and turning frame debugging on or off or taking what it collected. So are
//! `perf-option` ([`crate::perf_options`]), which then asks the event loop
//! for a redraw, and `perf-options`.

use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use smithay::reexports::calloop::{
    LoopHandle,
    channel::{Event as ChannelEvent, Sender, channel},
};
use tracing::{info, warn};

use crate::state::ShojiWM;

pub const SOCKET_ENV: &str = "SHOJIWM_SOCKET";

/// How long a connection may take to send its command line. Only a client
/// that connects and then says nothing ever waits this long.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Reload,
    FrameStats,
    FrameDebugOn,
    FrameDebugOff,
    FrameDebugTake,
    /// Turn a rendering shortcut on (`true`) or off.
    PerfOption(crate::perf_options::PerfOption, bool),
    PerfOptions,
}

impl Command {
    /// Answered from the socket's thread, with JSON.
    fn answers_with_json(self) -> bool {
        matches!(self, Self::FrameStats | Self::FrameDebugTake | Self::PerfOptions)
    }

    fn as_str(self) -> String {
        match self {
            Self::Quit => "quit".into(),
            Self::Reload => "reload".into(),
            Self::FrameStats => "frame-stats".into(),
            Self::FrameDebugOn => "frame-debug on".into(),
            Self::FrameDebugOff => "frame-debug off".into(),
            Self::FrameDebugTake => "frame-debug".into(),
            Self::PerfOption(option, on) => {
                format!("perf-option {} {}", option.name(), if on { "on" } else { "off" })
            }
            Self::PerfOptions => "perf-options".into(),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "quit" => Some(Self::Quit),
            "reload" => Some(Self::Reload),
            "frame-stats" => Some(Self::FrameStats),
            "frame-debug on" => Some(Self::FrameDebugOn),
            "frame-debug off" => Some(Self::FrameDebugOff),
            "frame-debug" => Some(Self::FrameDebugTake),
            "perf-options" => Some(Self::PerfOptions),
            other => {
                let (name, state) = other.strip_prefix("perf-option ")?.split_once(' ')?;
                let on = match state {
                    "on" => true,
                    "off" => false,
                    _ => return None,
                };
                Some(Self::PerfOption(crate::perf_options::PerfOption::parse(name)?, on))
            }
        }
    }
}

/// Where the compositor on `wayland_display` listens.
pub fn socket_path(wayland_display: &OsStr) -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty())?;
    let mut name = std::ffi::OsString::from("shojiwm.");
    name.push(wayland_display);
    name.push(".sock");
    Some(Path::new(&runtime_dir).join(name))
}

/// Removes the socket file when the compositor exits.
pub struct ControlSocket {
    path: PathBuf,
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the control socket for `wayland_display` and export its path. Failing
/// to bind only costs the CLI commands, so it is logged rather than fatal.
pub fn start(loop_handle: &LoopHandle<'static, ShojiWM>, wayland_display: &OsStr) -> Option<ControlSocket> {
    let Some(path) = socket_path(wayland_display) else {
        warn!("XDG_RUNTIME_DIR is not set; shoji_wm --quit/--reload will not work");
        return None;
    };
    // The path is namespaced by the Wayland display, which this process owns
    // now, so anything already there was left behind by a crashed instance.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to bind the control socket");
            return None;
        }
    };

    let (sender, receiver) = channel::<Command>();
    if let Err(error) = loop_handle.insert_source(receiver, |event, _, state| {
        if let ChannelEvent::Msg(command) = event {
            info!(command = %command.as_str(), "control socket command");
            match command {
                Command::Quit => state.shutdown(),
                Command::Reload => state.reload_decoration_runtime(),
                // Switched on the socket's thread; draw with it now.
                Command::PerfOption(..) => state.schedule_redraw(),
                Command::FrameStats
                | Command::FrameDebugOn
                | Command::FrameDebugOff
                | Command::FrameDebugTake
                | Command::PerfOptions => {}
            }
        }
    }) {
        warn!(%error, "failed to register the control socket channel");
        let _ = std::fs::remove_file(&path);
        return None;
    }

    let spawned = std::thread::Builder::new()
        .name("shojiwm-control".into())
        .spawn(move || serve(listener, sender));
    if let Err(error) = spawned {
        warn!(%error, "failed to start the control socket thread");
        let _ = std::fs::remove_file(&path);
        return None;
    }

    crate::process_env::set_var(SOCKET_ENV, &path);
    info!(path = %path.display(), "control socket listening");
    Some(ControlSocket { path })
}

fn serve(listener: UnixListener, sender: Sender<Command>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let reply = match read_command(&stream) {
            Ok(Command::FrameStats) => crate::frame_stats::json(),
            Ok(Command::FrameDebugTake) => crate::frame_stats::take_debug_json(),
            Ok(command @ (Command::FrameDebugOn | Command::FrameDebugOff)) => {
                crate::frame_stats::set_debugging(command == Command::FrameDebugOn);
                info!(command = %command.as_str(), "control socket command");
                "ok".to_string()
            }
            Ok(Command::PerfOptions) => crate::perf_options::json(),
            Ok(command @ Command::PerfOption(option, on)) => {
                option.set(on);
                match sender.send(command) {
                    Ok(()) => "ok".to_string(),
                    Err(_) => return,
                }
            }
            Ok(command) => match sender.send(command) {
                Ok(()) => "ok".to_string(),
                // The event loop is gone: the compositor is shutting down.
                Err(_) => return,
            },
            Err(error) => format!("error: {error}"),
        };
        let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
    }
}

fn read_command(stream: &UnixStream) -> Result<Command, String> {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    Command::parse(&line).ok_or_else(|| format!("unknown command {:?}", line.trim()))
}

/// Client side: send `command` to the running compositor. `Some` carries a
/// reply with content (`frame-stats`, `frame-debug`).
///
/// The socket comes from `SHOJIWM_SOCKET`, or failing that from
/// `WAYLAND_DISPLAY`, so a shell started inside the session finds it either way.
pub fn send(command: Command) -> Result<Option<String>, String> {
    let path = std::env::var_os(SOCKET_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("WAYLAND_DISPLAY")
                .filter(|display| !display.is_empty())
                .and_then(|display| socket_path(&display))
        })
        .ok_or_else(|| {
            format!("no running ShojiWM found ({SOCKET_ENV} and WAYLAND_DISPLAY are unset)")
        })?;
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("cannot connect to {}: {error}", path.display()))?;
    stream
        .write_all(format!("{}\n", command.as_str()).as_bytes())
        .map_err(|error| error.to_string())?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|error| error.to_string())?;
    match reply.trim() {
        "ok" => Ok(None),
        "" => Err("the compositor closed the connection without answering".to_string()),
        json if command.answers_with_json() && json.starts_with('{') => Ok(Some(json.to_string())),
        other => Err(other.strip_prefix("error: ").unwrap_or(other).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_through_the_wire_format() {
        for command in [
            Command::Quit,
            Command::Reload,
            Command::FrameStats,
            Command::FrameDebugOn,
            Command::FrameDebugOff,
            Command::FrameDebugTake,
            Command::PerfOption(crate::perf_options::PerfOption::TargetedRedraw, false),
            Command::PerfOption(crate::perf_options::PerfOption::FrameGate, true),
            Command::PerfOptions,
        ] {
            assert_eq!(Command::parse(&format!("{}\n", command.as_str())), Some(command));
        }
        assert_eq!(Command::parse("frame-debug maybe"), None);
    }
}
