//! X11 backend.
//!
//! X11 rather than Wayland because that is what the plugin formats speak:
//! VST3's `X11EmbedWindowID` and CLAP's `x11` both name an X window id, and
//! neither format has a Wayland handle to hand over. Under a Wayland session the
//! DAW and the plugins are running on XWayland for the same reason.
//!
//! The connection is ours, not the DAW's, so nothing the host pumps advances it
//! — see [`conn`].

mod conn;
mod keys;
mod window;

pub(crate) use keys::forward_key;
pub(crate) use window::{Window, root_window};

/// Whether the host, rather than this crate, drives the event source our
/// windows live on. See [`crate::poll`].
pub(crate) const HOST_DRIVES_EVENTS: bool = false;

pub(crate) fn pump_events() {
    conn::pump();
}

/// Display scale, read from the X resource database.
///
/// `Xft.dpi` is what every toolkit on X uses and what the desktop's own
/// settings write, so it is the one answer the plugin will agree with. There is
/// no per-window figure to have: X reports a physical size per screen, and under
/// XWayland and most multi-monitor setups that number is invented.
fn scale_factor(conn: &conn::Conn) -> f64 {
    let Some(dpi) = xft_dpi(conn) else { return 1.0 };
    if dpi > 0.0 { dpi / 96.0 } else { 1.0 }
}

fn xft_dpi(conn: &conn::Conn) -> Option<f64> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let reply = conn
        .conn
        .get_property(
            false,
            conn.root(),
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            // The database is a few kilobytes at most, and this is read once.
            16 * 1024,
        )
        .ok()?
        .reply()
        .ok()?;

    let database = String::from_utf8_lossy(&reply.value);
    parse_xft_dpi(&database)
}

/// `Xft.dpi` out of an X resource database, if it names one.
fn parse_xft_dpi(database: &str) -> Option<f64> {
    database
        .lines()
        .filter_map(|line| line.strip_prefix("Xft.dpi:"))
        .filter_map(|value| value.trim().parse().ok())
        .next()
}

#[cfg(test)]
mod tests {
    use super::parse_xft_dpi;

    #[test]
    fn the_dpi_is_read_out_of_the_database() {
        let database = "*background:\t#1d1f21\nXft.dpi:\t144\nXft.antialias:\t1\n";
        assert_eq!(parse_xft_dpi(database), Some(144.0));
    }

    #[test]
    fn a_database_without_a_dpi_says_nothing() {
        assert_eq!(parse_xft_dpi("Xft.antialias:\t1\n"), None);
        // Not a prefix match on some other resource that starts the same way.
        assert_eq!(parse_xft_dpi("Xft.dpiFoo:\tbad\n"), None);
    }
}
