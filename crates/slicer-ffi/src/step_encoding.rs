//! OCCT 7.6 writes UTF-8 bytes into Part 21 string literals. All syntax
//! outside those literals, and OCCT's own quote and reverse-solidus escapes,
//! is already ASCII.
use std::io::{self, BufRead, Write};

pub(super) fn encode_step_file(path: &str) -> io::Result<()> {
    use std::fs::File;
    use std::io::{BufReader, BufWriter};
    use std::path::Path;

    let mut builder = tempfile::Builder::new();
    builder.suffix(".step");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o644));
    }
    let mut encoded = builder.tempfile_in(Path::new(path).parent().unwrap_or(Path::new(".")))?;
    {
        let mut output = BufWriter::new(encoded.as_file_mut());
        encode_step(BufReader::new(File::open(path)?), &mut output)?;
        output.flush()?;
    }
    encoded.persist(path)?;
    Ok(())
}

fn encode_literal(bytes: &[u8], output: &mut impl Write) -> io::Result<()> {
    let text =
        std::str::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut width = 0;
    for ch in text.chars() {
        let digits = match ch {
            ' '..='~' => 0,
            '\0'..='\u{ffff}' => 4,
            _ => 8,
        };
        if digits != width {
            if width != 0 {
                output.write_all(br"\X0\")?;
            }
            match digits {
                4 => output.write_all(br"\X2\")?,
                8 => output.write_all(br"\X4\")?,
                _ => {}
            }
            width = digits;
        }
        if width == 0 {
            output.write_all(&[ch as u8])?;
        } else {
            write!(output, "{:0width$X}", ch as u32)?;
        }
    }
    if width != 0 {
        output.write_all(br"\X0\")?;
    }
    Ok(())
}

fn write_syntax(bytes: &[u8], output: &mut impl Write) -> io::Result<()> {
    if !bytes.is_ascii() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-ASCII outside STEP literal",
        ));
    }
    output.write_all(bytes)
}

fn encode_step(mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    #[derive(PartialEq)]
    enum State {
        Syntax,
        Comment,
        Literal,
    }

    let mut state = State::Syntax;
    let mut line = Vec::new();
    let mut literal = Vec::new();
    while input.read_until(b'\n', &mut line)? != 0 {
        let body = line
            .strip_suffix(b"\r\n")
            .or_else(|| line.strip_suffix(b"\n"))
            .unwrap_or(&line);
        let mut start = 0;
        let mut i = 0;
        while i < body.len() {
            match (&state, body[i]) {
                (State::Syntax, b'\'') => {
                    write_syntax(&body[start..=i], &mut output)?;
                    start = i + 1;
                    state = State::Literal;
                }
                (State::Literal, b'\'') => {
                    literal.extend_from_slice(&body[start..i]);
                    encode_literal(&literal, &mut output)?;
                    literal.clear();
                    start = i; // copy the closing quote with the syntax that follows
                    state = State::Syntax;
                }
                (State::Syntax, b'/') if body.get(i + 1) == Some(&b'*') => {
                    state = State::Comment;
                    i += 1;
                }
                (State::Comment, b'*') if body.get(i + 1) == Some(&b'/') => {
                    state = State::Syntax;
                    i += 1;
                }
                _ => {}
            }
            i += 1;
        }
        if state == State::Literal {
            // StepData_StepWriter can wrap even between bytes of a UTF-8 scalar.
            literal.extend_from_slice(&body[start..]);
        } else if &body[start..] == b"'" {
            // The next line can start with the second half of a doubled quote.
            output.write_all(b"'")?;
        } else {
            write_syntax(&line[start..], &mut output)?;
        }
        line.clear();
    }
    if state != State::Syntax {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unterminated STEP literal or comment",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_comment_and_wrapped_quote_and_backslash_pairs() {
        let input = b"/* file's\n\tcomment */ ('Mike'\n's\\\r\n\\bolt');\n";
        let mut result = Vec::new();
        encode_step(&input[..], &mut result).unwrap();
        assert_eq!(result, b"/* file's\n\tcomment */ ('Mike''s\\\\bolt');\n");
    }

    #[test]
    fn preserves_ascii_escapes_and_encodes_wrapped_utf8() {
        let mut source = r"/* OCCT */ PRODUCT('Mike''s\\Корпус".as_bytes().to_vec();
        source.extend_from_slice(&[0xf0, 0x9f, b'\r', b'\n', 0x94, 0xa9]);
        source.extend_from_slice("Я!');\r\n".as_bytes());
        let mut result = Vec::new();
        encode_step(&source[..], &mut result).unwrap();
        assert_eq!(
            std::str::from_utf8(&result).unwrap(),
            concat!(
                r"/* OCCT */ PRODUCT('Mike''s\\\X2\041A043E0440043F04430441\X0\",
                r"\X4\0001F529\X0\\X2\042F\X0\!');",
                "\r\n"
            )
        );
    }
}
