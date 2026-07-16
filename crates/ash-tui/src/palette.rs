use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) type Rgb = (u8, u8, u8);

pub(crate) fn composer_background_color() -> Option<Rgb> {
    probe_background(PROBE_TIMEOUT).map(composer_background)
}

fn composer_background(terminal_background: Rgb) -> Rgb {
    let luminance = 0.299 * f32::from(terminal_background.0)
        + 0.587 * f32::from(terminal_background.1)
        + 0.114 * f32::from(terminal_background.2);
    let (overlay, alpha) = if luminance > 128.0 {
        ((0, 0, 0), 0.04)
    } else {
        ((255, 255, 255), 0.12)
    };
    blend(overlay, terminal_background, alpha)
}

fn blend(foreground: Rgb, background: Rgb, alpha: f32) -> Rgb {
    let channel = |foreground: u8, background: u8| {
        (f32::from(foreground) * alpha + f32::from(background) * (1.0 - alpha)) as u8
    };
    (
        channel(foreground.0, background.0),
        channel(foreground.1, background.1),
        channel(foreground.2, background.2),
    )
}

#[cfg(unix)]
fn probe_background(timeout: Duration) -> Option<Rgb> {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    struct NonBlockingReader {
        file: std::fs::File,
        original_flags: libc::c_int,
    }

    impl Drop for NonBlockingReader {
        fn drop(&mut self) {
            unsafe {
                libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, self.original_flags);
            }
        }
    }

    let reader = OpenOptions::new().read(true).open("/dev/tty").ok()?;
    let mut writer = OpenOptions::new().write(true).open("/dev/tty").ok()?;
    let fd = reader.as_raw_fd();
    let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if original_flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } == -1
    {
        return None;
    }
    let mut reader = NonBlockingReader {
        file: reader,
        original_flags,
    };

    writer.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\").ok()?;
    writer.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0_u8; 256];
        loop {
            match reader.file.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(_) => return None,
            }
        }
        if let Some((_, background)) = parse_default_colors(&buffer) {
            return Some(background);
        }

        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(libc::c_int::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, remaining_ms) };
        if result <= 0 {
            return None;
        }
    }
}

#[cfg(not(unix))]
fn probe_background(_timeout: Duration) -> Option<Rgb> {
    None
}

fn parse_osc_color(buffer: &[u8], slot: u8) -> Option<Rgb> {
    let prefix = format!("\x1b]{slot};");
    let start = find_subslice(buffer, prefix.as_bytes())? + prefix.len();
    let rest = &buffer[start..];
    let end = rest
        .iter()
        .enumerate()
        .find_map(|(index, byte)| match byte {
            0x07 => Some(index),
            0x1b if rest.get(index + 1) == Some(&b'\\') => Some(index),
            _ => None,
        })?;
    parse_osc_rgb(std::str::from_utf8(&rest[..end]).ok()?)
}

fn parse_default_colors(buffer: &[u8]) -> Option<(Rgb, Rgb)> {
    parse_osc_color(buffer, 10).zip(parse_osc_color(buffer, 11))
}

fn parse_osc_rgb(value: &str) -> Option<Rgb> {
    let (kind, values) = value.trim().split_once(':')?;
    if !kind.eq_ignore_ascii_case("rgb") && !kind.eq_ignore_ascii_case("rgba") {
        return None;
    }
    let mut parts = values.split('/');
    let red = parse_component(parts.next()?)?;
    let green = parse_component(parts.next()?)?;
    let blue = parse_component(parts.next()?)?;
    if kind.eq_ignore_ascii_case("rgba") {
        parse_component(parts.next()?)?;
    }
    parts.next().is_none().then_some((red, green, blue))
}

fn parse_component(value: &str) -> Option<u8> {
    match value.len() {
        2 => u8::from_str_radix(value, 16).ok(),
        4 => u16::from_str_radix(value, 16)
            .ok()
            .map(|value| (value / 257) as u8),
        _ => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_background_colors() {
        assert_eq!(
            parse_osc_color(b"\x1b]11;rgb:ffff/8000/0000\x07", 11),
            Some((255, 127, 0))
        );
        assert_eq!(
            parse_osc_color(b"noise\x1b]11;rgba:00/80/ff/ff\x1b\\", 11),
            Some((0, 128, 255))
        );
    }

    #[test]
    fn requires_foreground_and_background_colors() {
        assert_eq!(
            parse_default_colors(b"\x1b]11;rgb:1111/1111/1111\x07\x1b]10;rgb:eeee/eeee/eeee\x1b\\"),
            Some(((238, 238, 238), (17, 17, 17)))
        );
        assert_eq!(
            parse_default_colors(b"\x1b]11;rgb:1111/1111/1111\x07"),
            None
        );
    }

    #[test]
    fn composer_background_matches_codex_blending() {
        assert_eq!(composer_background((0, 0, 0)), (30, 30, 30));
        assert_eq!(composer_background((255, 255, 255)), (244, 244, 244));
        assert_eq!(composer_background((30, 30, 30)), (57, 57, 57));
    }
}
