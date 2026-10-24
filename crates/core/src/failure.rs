//! Why a track could not be read: a short machine code for the UI to translate, plus
//! a sanitized message for whoever investigates.

use crate::source::SourceError;
use crate::Error;

/// Longest stored failure message, in characters.
pub const MESSAGE_MAX_CHARS: usize = 200;

/// What kind of failure it was; stored as [`FailCode::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailCode {
    NotFound,
    Timeout,
    Unreachable,
    Decode,
    Unsupported,
    Tag,
    Io,
}

impl FailCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FailCode::NotFound => "not_found",
            FailCode::Timeout => "timeout",
            FailCode::Unreachable => "unreachable",
            FailCode::Decode => "decode",
            FailCode::Unsupported => "unsupported",
            FailCode::Tag => "tag",
            FailCode::Io => "io",
        }
    }
}

/// A failure to open, tag or decode a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: FailCode,
    /// Sanitized (see [`sanitize_message`]).
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Failure {
    pub fn new(code: FailCode, message: &str) -> Self {
        Self {
            code,
            message: sanitize_message(message),
        }
    }

    /// Classifies a playback (open or decode) error.
    pub fn from_error(e: &Error) -> Self {
        let code = match e {
            Error::Source(s) => source_code(s),
            Error::Io(io) => io_code(io),
            Error::Decode(msg) if msg.to_ascii_lowercase().contains("unsupported") => {
                FailCode::Unsupported
            }
            Error::Decode(_) => FailCode::Decode,
            Error::Db(_) | Error::Other(_) => FailCode::Io,
        };
        Self::new(code, &e.to_string())
    }

    /// Classifies an error while reading tags during a scan: source and network
    /// trouble keep their codes; anything else (a short or malformed file, say) is a
    /// tag failure.
    pub fn from_tag_error(e: &Error) -> Self {
        let keep = match e {
            Error::Source(_) => true,
            Error::Io(io) => io_code(io) != FailCode::Io,
            _ => false,
        };
        if keep {
            Self::from_error(e)
        } else {
            Self::new(FailCode::Tag, &e.to_string())
        }
    }
}

fn source_code(e: &SourceError) -> FailCode {
    match e {
        SourceError::NotFound => FailCode::NotFound,
        SourceError::Timeout => FailCode::Timeout,
        SourceError::Unreachable | SourceError::Auth => FailCode::Unreachable,
        SourceError::Io(_) => FailCode::Io,
    }
}

fn io_code(e: &std::io::Error) -> FailCode {
    use std::io::ErrorKind::*;
    match e.kind() {
        TimedOut => FailCode::Timeout,
        NotFound => FailCode::NotFound,
        NotConnected | ConnectionReset | ConnectionAborted | ConnectionRefused | BrokenPipe => {
            FailCode::Unreachable
        }
        _ => FailCode::Io,
    }
}

/// One line, at most [`MESSAGE_MAX_CHARS`] characters, with any `user:password@`
/// (or `user@`) after `//` removed so no credentials are stored or shown.
pub fn sanitize_message(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len().min(MESSAGE_MAX_CHARS * 4));
    let mut rest = msg;
    while let Some(i) = rest.find("//") {
        out.push_str(&rest[..i + 2]);
        rest = &rest[i + 2..];
        let authority_end = rest
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    let one_line: String = out
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= MESSAGE_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut short: String = trimmed.chars().take(MESSAGE_MAX_CHARS - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn source_errors_get_their_codes() {
        let code = |e: SourceError| Failure::from_error(&Error::Source(e)).code;
        assert_eq!(code(SourceError::NotFound), FailCode::NotFound);
        assert_eq!(code(SourceError::Timeout), FailCode::Timeout);
        assert_eq!(code(SourceError::Unreachable), FailCode::Unreachable);
        assert_eq!(code(SourceError::Auth), FailCode::Unreachable);
        assert_eq!(code(SourceError::Io("x".into())), FailCode::Io);
    }

    #[test]
    fn io_and_decode_errors_get_their_codes() {
        let io = |k: ErrorKind| Failure::from_error(&Error::Io(IoError::new(k, "x"))).code;
        assert_eq!(io(ErrorKind::TimedOut), FailCode::Timeout);
        assert_eq!(io(ErrorKind::ConnectionReset), FailCode::Unreachable);
        assert_eq!(io(ErrorKind::NotFound), FailCode::NotFound);
        assert_eq!(io(ErrorKind::PermissionDenied), FailCode::Io);
        let dec = |m: &str| Failure::from_error(&Error::Decode(m.into())).code;
        assert_eq!(
            dec("unsupported feature: core (probe)"),
            FailCode::Unsupported
        );
        assert_eq!(dec("malformed stream: bad sync"), FailCode::Decode);
        assert_eq!(FailCode::NotFound.as_str(), "not_found");
        assert_eq!(FailCode::Unsupported.as_str(), "unsupported");
    }

    #[test]
    fn tag_errors_keep_network_codes() {
        let f = Failure::from_tag_error(&Error::Decode("no tag".into()));
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailCode::Tag, "decode error: no tag")
        );
        let f = Failure::from_tag_error(&Error::Io(IoError::new(ErrorKind::TimedOut, "t")));
        assert_eq!(f.code, FailCode::Timeout);
        let f = Failure::from_tag_error(&Error::Source(SourceError::NotFound));
        assert_eq!(f.code, FailCode::NotFound);
        let f = Failure::from_tag_error(&Error::Io(IoError::new(ErrorKind::UnexpectedEof, "eof")));
        assert_eq!(f.code, FailCode::Tag);
    }

    #[test]
    fn messages_are_short_one_line_and_without_credentials() {
        assert_eq!(
            sanitize_message("open smb://music:s3cret@10.0.0.1/F/a.mp3 failed"),
            "open smb://10.0.0.1/F/a.mp3 failed"
        );
        assert_eq!(
            sanitize_message("//user@host/x and //other@h2"),
            "//host/x and //h2"
        );
        assert_eq!(sanitize_message("a\nb\tc "), "a b c");
        assert_eq!(sanitize_message("no // at sign"), "no // at sign");
        let long = sanitize_message(&"ž".repeat(500));
        assert_eq!(long.chars().count(), MESSAGE_MAX_CHARS);
        assert!(long.ends_with('…'));
    }
}
