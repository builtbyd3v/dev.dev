//! Auto-opens a browser pane when a new local dev server starts listening on a port, even if the
//! server was started as a background task and never produced PTY output (so the existing
//! output-scan detection in `terminal::model::terminal_model` -- which greps the block's stdout
//! for a `localhost:PORT`-shaped URL -- never fires). This is the slow-but-reliable fallback:
//! output-scan is the instant path when it works, this poll loop is the safety net.
//!
//! Implemented on Windows (via `GetExtendedTcpTable`) and macOS (via `lsof`); other platforms
//! just never produce new ports and the feature is effectively a no-op there. Wired into
//! [`crate::workspace::view::WorkspaceView`], which owns a
//! [`PortWatcherState`] and re-schedules a poll of [`snapshot_listening_ports`] every
//! [`POLL_INTERVAL`] via the app's normal `ctx.spawn` + `Timer::after` idiom (see
//! `schedule_next_port_watcher_poll`).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::ops::RangeInclusive;
use std::time::Duration;

/// ponytail: kill switch until this graduates into a real user-facing setting (e.g.
/// `AutoOpenDevServerBrowserPaneSetting`, matching the one already stubbed for the output-scan
/// path in `terminal::view`'s `DevServerUrlDetected` handler).
pub const AUTO_OPEN_PORT_WATCHER: bool = true;

/// How often to re-enumerate listening ports.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Ports below this are system/well-known services, not dev servers.
const DEV_PORT_RANGE: RangeInclusive<u16> = 1024..=65535;

/// Well-known non-web services that fall inside [`DEV_PORT_RANGE`] and would otherwise pop a
/// browser pane every time a local database/broker starts up. Deliberately a flat list, not a
/// config -- keep it simple, extend as false positives show up.
const EXCLUDED_PORTS: &[u16] = &[
    5432,  // postgres
    3306,  // mysql
    6379,  // redis
    27017, // mongodb
    9200, 9300, // elasticsearch
    2181,  // zookeeper
    9092,  // kafka
    5672, 15672, // rabbitmq
    11211, // memcached
    1433,  // mssql
];

fn is_candidate_port(port: u16) -> bool {
    DEV_PORT_RANGE.contains(&port) && !EXCLUDED_PORTS.contains(&port)
}

/// Tracks already-triggered ports so a restart (port disappears, then comes back) re-triggers an
/// auto-open, but repeated polls of an already-handled port don't. Also absorbs the startup
/// baseline: ports already listening when Warp starts watching are never "new".
#[derive(Default)]
pub struct PortWatcherState {
    /// Candidate ports considered already-handled: either part of the startup baseline, already
    /// auto-opened, or already opened via the instant output-scan path (see
    /// `mark_handled_externally`). Removed when the port stops listening, so a later restart on
    /// the same port re-triggers.
    handled: HashSet<u16>,
}

impl PortWatcherState {
    /// Snapshots currently-listening candidate ports as the startup baseline -- none of these
    /// should trigger an auto-open, since they were already running before Warp started
    /// watching.
    pub fn with_startup_baseline() -> Self {
        Self {
            handled: snapshot_listening_ports(),
        }
    }

    /// Diffs `current` against the handled set, returning ports that are newly listening.
    /// Ports that stopped listening are dropped from `handled` so a later restart re-triggers.
    /// Callers should insert newly-returned ports back into `handled` once they've decided what
    /// to do with them (probe, then auto-open or drop) -- see
    /// `WorkspaceView::handle_port_watcher_tick`.
    pub fn diff_new_ports(&mut self, current: &HashSet<u16>) -> Vec<u16> {
        self.handled.retain(|port| current.contains(port));
        current.difference(&self.handled).copied().collect()
    }

    /// Marks a port as handled (whether that's "auto-opened", "probe failed, don't retry until
    /// it cycles", or "the instant output-scan path already opened a pane for it" -- see the
    /// module doc's note on dedup with the grid-scan path).
    pub fn mark_handled(&mut self, port: u16) {
        self.handled.insert(port);
    }
}

/// Quick "is anything actually there" check. Per the deliberately dumb spec: a bare TCP connect
/// counts as "webby enough" -- we don't parse a response or even confirm it's HTTP. A
/// bound-but-not-yet-accepting socket (e.g. mid-startup) fails this and gets re-tried on the next
/// poll since it's not added to `handled` until this succeeds.
pub fn probe_port(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

#[cfg(target_os = "windows")]
pub fn snapshot_listening_ports() -> HashSet<u16> {
    windows_impl::snapshot_listening_ports()
}

#[cfg(target_os = "macos")]
pub fn snapshot_listening_ports() -> HashSet<u16> {
    macos_impl::snapshot_listening_ports()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn snapshot_listening_ports() -> HashSet<u16> {
    // No enumeration API wired up for this platform yet -- the port watcher becomes a no-op
    // (never observes new ports) rather than a compile error. Output-scan detection still works.
    HashSet::new()
}

#[cfg(target_os = "macos")]
mod macos_impl {
    use std::collections::HashSet;
    use std::process::Command;

    use super::is_candidate_port;

    /// Enumerates ports with a bound LISTEN socket by shelling out to `lsof`, filtered to
    /// [`super::is_candidate_port`]. `lsof` ships with macOS, so no crate/FFI is pulled in for
    /// what runs once every `POLL_INTERVAL`.
    ///
    /// ponytail: shells out + parses text instead of libproc FFI (`proc_pidfdinfo` /
    /// `PROC_PIDFDSOCKETINFO`). Move to libproc only if `lsof`'s ~tens-of-ms process spawn shows
    /// up on the poll loop.
    pub(super) fn snapshot_listening_ports() -> HashSet<u16> {
        // `-nP` skips host/port name resolution (faster, and keeps ports numeric); `-iTCP
        // -sTCP:LISTEN` restricts to listening TCP sockets; `-F n` prints just the NAME field
        // one per line (e.g. `n127.0.0.1:3000`, `n[::1]:8080`, `n*:5173`).
        let output = match Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "n"])
            .output()
        {
            Ok(output) => output,
            Err(err) => {
                log::warn!("port_watcher: failed to run lsof: {err}");
                return HashSet::new();
            }
        };

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('n'))
            // Port is whatever follows the last colon (`host:port`); rsplit handles the IPv6
            // `[::1]:PORT` form since the port colon is always the last one.
            .filter_map(|name| name.rsplit_once(':').map(|(_, port)| port))
            .filter_map(|port| port.parse::<u16>().ok())
            .filter(|port| is_candidate_port(*port))
            .collect()
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::collections::HashSet;
    use std::mem::size_of;

    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows::Win32::Networking::WinSock::AF_INET;

    use super::is_candidate_port;

    /// Enumerates ports with a bound LISTEN socket via `GetExtendedTcpTable`, filtered to
    /// [`super::is_candidate_port`]. Blocking (a couple of syscalls); cheap enough to call every
    /// `POLL_INTERVAL` from the main thread's poll loop -- there's no long-running I/O here, just
    /// a kernel table dump.
    pub(super) fn snapshot_listening_ports() -> HashSet<u16> {
        let mut buf: Vec<u8> = Vec::new();
        let mut size: u32 = 0;

        // First call with an empty buffer to learn the required size (returns
        // ERROR_INSUFFICIENT_BUFFER and fills `size`), then retry once allocated. Loop in case
        // the table grows between the two calls (rare, but cheap to handle).
        for _ in 0..3 {
            let result = unsafe {
                GetExtendedTcpTable(
                    Some(buf.as_mut_ptr().cast()),
                    &mut size,
                    false,
                    AF_INET.0 as u32,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == NO_ERROR.0 {
                break;
            }
            if result != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
                log::warn!("port_watcher: GetExtendedTcpTable failed with {result}");
                return HashSet::new();
            }
            buf = vec![0u8; size as usize];
        }

        if buf.is_empty() || (size as usize) < size_of::<MIB_TCPTABLE_OWNER_PID>() {
            return HashSet::new();
        }

        // SAFETY: `buf` was sized by the syscall above per its documented layout: a
        // `dwNumEntries: u32` header followed by that many `MIB_TCPROW_OWNER_PID` entries.
        let table = unsafe { &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID) };
        let num_entries = table.dwNumEntries as usize;
        let rows: &[MIB_TCPROW_OWNER_PID] =
            unsafe { std::slice::from_raw_parts(table.table.as_ptr(), num_entries) };

        rows.iter()
            .filter(|row| row.dwState == MIB_TCP_STATE_LISTEN.0 as u32)
            .filter_map(|row| {
                // dwLocalPort is stored in network byte order (big-endian) in the low 16 bits.
                let port = u16::from_be(row.dwLocalPort as u16);
                is_candidate_port(port).then_some(port)
            })
            .collect()
    }
}
