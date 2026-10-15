use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

const BELL_CHUNK: [u8; 64] = [b'\x07'; 64];

pub(crate) fn write_terminal_bells<W: Write>(writer: &mut W, count: u16) -> io::Result<()> {
    let full_chunks = usize::from(count) / BELL_CHUNK.len();
    let remainder = usize::from(count) % BELL_CHUNK.len();
    for _ in 0..full_chunks {
        writer.write_all(&BELL_CHUNK)?;
    }
    writer.write_all(&BELL_CHUNK[..remainder])?;
    writer.flush()
}

pub(crate) fn write_window_title<W: Write>(writer: &mut W, title: Option<&str>) -> io::Result<()> {
    let title = title.unwrap_or("herdr");
    let safe_title = title
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .collect::<String>();
    write!(writer, "\x1b]0;{safe_title}\x07")?;
    writer.flush()
}

/// Whether herdr changed the mouse pointer, so exit knows to hand it back.
static POINTER_CHANGED: AtomicBool = AtomicBool::new(false);

/// Asks the terminal for a mouse pointer by its CSS name (OSC 22), such as
/// "col-resize" or "pointer"; an empty name hands it back to the terminal.
/// Terminals without OSC 22 ignore it.
pub(crate) fn write_pointer_shape<W: Write>(writer: &mut W, shape: &str) -> io::Result<()> {
    POINTER_CHANGED.store(!shape.is_empty(), Ordering::Relaxed);
    write!(writer, "\x1b]22;{shape}\x1b\\")?;
    writer.flush()
}

pub(crate) fn reset_pointer_shape<W: Write>(writer: &mut W) -> io::Result<()> {
    if POINTER_CHANGED.load(Ordering::Relaxed) {
        write_pointer_shape(writer, "")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_shapes_are_osc_22_and_reset_only_after_a_change() {
        let mut out = Vec::new();
        write_pointer_shape(&mut out, "col-resize").expect("write");
        assert_eq!(out, b"\x1b]22;col-resize\x1b\\");
        out.clear();
        reset_pointer_shape(&mut out).expect("reset");
        assert_eq!(out, b"\x1b]22;\x1b\\");
        out.clear();
        reset_pointer_shape(&mut out).expect("reset again");
        assert!(out.is_empty());
    }

    #[test]
    fn writes_exact_terminal_bell_count() {
        let mut output = Vec::new();

        write_terminal_bells(&mut output, 130).unwrap();

        assert_eq!(output, vec![b'\x07'; 130]);
    }

    #[test]
    fn window_title_strips_terminators_and_defaults_to_herdr() {
        let mut output = Vec::new();
        write_window_title(&mut output, Some("herdr\x1b api\u{7}\u{9c}")).unwrap();
        assert_eq!(output, b"\x1b]0;herdr api\x07");

        output.clear();
        write_window_title(&mut output, None).unwrap();
        assert_eq!(output, b"\x1b]0;herdr\x07");
    }
}
