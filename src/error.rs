use std::fmt;

pub enum ProgramError {
    NoPortFound,
    UnableToOpen(String, std::io::Error),
    IoError(std::io::Error),
    SerialPortError(mio_serial::Error),
}

impl std::error::Error for ProgramError {}

impl From<std::io::Error> for ProgramError {
    fn from(err: std::io::Error) -> ProgramError {
        ProgramError::IoError(err)
    }
}

impl From<mio_serial::Error> for ProgramError {
    fn from(err: mio_serial::Error) -> ProgramError {
        ProgramError::SerialPortError(err)
    }
}

impl fmt::Debug for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProgramError::NoPortFound => {
                write!(f, "No USB serial adapter found which matches criteria.")
            }
            ProgramError::UnableToOpen(port_name, err) => {
                write!(f, "Unable to open serial port '{}': {}", port_name, err)
            }
            ProgramError::IoError(err) => write!(f, "{}", err),
            ProgramError::SerialPortError(err) => write!(f, "SerialPortError: {}", err),
        }
    }
}
impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}
pub type Result<T> = std::result::Result<T, ProgramError>;

#[cfg(test)]
mod tests {
    use super::ProgramError;

    #[test]
    fn messages_name_the_failure() {
        assert_eq!(
            ProgramError::NoPortFound.to_string(),
            "No USB serial adapter found which matches criteria."
        );
        assert_eq!(
            ProgramError::UnableToOpen("COM1".to_string(), std::io::Error::other("busy"))
                .to_string(),
            "Unable to open serial port 'COM1': busy"
        );
        assert_eq!(
            ProgramError::IoError(std::io::Error::other("disk")).to_string(),
            "disk"
        );
        assert_eq!(
            ProgramError::SerialPortError(mio_serial::Error::new(
                mio_serial::ErrorKind::Unknown,
                "boom"
            ))
            .to_string(),
            "SerialPortError: boom"
        );
    }

    #[test]
    fn io_and_serial_errors_convert_into_program_errors() {
        let io_err: ProgramError = std::io::Error::other("disk").into();
        assert_eq!(io_err.to_string(), "disk");
        let serial: ProgramError =
            mio_serial::Error::new(mio_serial::ErrorKind::NoDevice, "none").into();
        assert_eq!(serial.to_string(), "SerialPortError: none");
    }
}
