use std::io::Write;

use crate::{cli::Format, error::AppError, report::Report};

pub(crate) fn report(
    writer: &mut impl Write,
    report: &Report,
    format: Format,
) -> Result<(), AppError> {
    let bytes = match format {
        Format::Text => crate::report::text(report).into_bytes(),
        Format::Json => {
            let mut bytes = serde_json::to_vec(report)?;
            bytes.push(b'\n');
            bytes
        }
    };
    write_bytes(writer, &bytes)
}

pub(crate) fn line(writer: &mut impl Write, text: &str) -> Result<(), AppError> {
    write_bytes(writer, format!("{text}\n").as_bytes())
}

pub(crate) fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<(), AppError> {
    writer.write_all(bytes).map_err(AppError::Output)?;
    writer.flush().map_err(AppError::Output)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use super::write_bytes;

    struct ShortWriter {
        bytes: Vec<u8>,
        flush_error: Option<io::ErrorKind>,
    }

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let size = bytes.len().min(2);
            self.bytes.extend_from_slice(&bytes[..size]);
            Ok(size)
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.flush_error {
                Some(kind) => Err(kind.into()),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn short_writes_are_completed_and_final_flush_is_observed() {
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            flush_error: Some(io::ErrorKind::PermissionDenied),
        };
        let error = write_bytes(&mut writer, b"{\"a\":1}\n").unwrap_err();
        assert_eq!(writer.bytes, b"{\"a\":1}\n");
        assert!(!error.is_stdout_closed());
        writer.flush_error = Some(io::ErrorKind::BrokenPipe);
        assert!(
            write_bytes(&mut writer, b"x")
                .unwrap_err()
                .is_stdout_closed()
        );
    }
}
