//! The `host` label.
//!
//! The label value has to be the machine's real name or nothing at all: a
//! placeholder like "unknown" is a value the series would carry forever and
//! that a join could match on. If the name cannot be read, or is not something
//! that can be written into an exposition line, the series publish without a
//! `host` label and the exporter says so once, at startup.

/// The hostname, or `None` if it cannot be read or is unusable as a label.
#[cfg(unix)]
pub fn detect() -> Option<String> {
    const LEN: usize = 256;
    let mut buffer = [0 as libc::c_char; LEN];
    // SAFETY: `buffer` is a live, writable array of exactly LEN elements and
    // the length passed is LEN - 1, so gethostname always has room for its
    // NUL terminator inside the array. The pointer does not outlive the call.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr(), LEN - 1) };
    if result != 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    usable(String::from_utf8(bytes).ok()?)
}

#[cfg(not(unix))]
pub fn detect() -> Option<String> {
    None
}

/// A label value has to survive being written into a line of exposition. The
/// escape set Prometheus defines is backslash, quote and newline; any other
/// control byte would break the line and there is no escape for it, so such a
/// name is refused rather than mangled into something that is not the name.
pub fn usable(name: String) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_control_bearing_names_are_refused() {
        assert_eq!(usable(String::new()), None);
        assert_eq!(usable("bad\rname".to_owned()), None);
        assert_eq!(usable("bad\u{7f}name".to_owned()), None);
    }

    #[test]
    fn escapable_characters_are_kept_for_the_renderer_to_escape() {
        assert_eq!(
            usable("od\"d\\name\nline".to_owned()),
            Some("od\"d\\name\nline".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn detect_returns_a_usable_name_or_nothing() {
        // Asserts only the contract, never a particular machine's name.
        if let Some(name) = detect() {
            assert!(!name.is_empty());
            assert_eq!(usable(name.clone()), Some(name));
        }
    }
}
