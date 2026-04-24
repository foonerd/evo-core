//! Exit status codes (PLUGIN_TOOL.md section 8): 0=ok, 1=usage, 2=trust/sig, 3=io, 4=network

use std::io;

use evo_trust::TrustError;

/// Map an error to exit code: trust 2, ureq 4, io 3, other 1
pub fn code_from_error(e: &anyhow::Error) -> u8 {
    if e.is::<TrustError>() {
        2
    } else if e.is::<ureq::Error>() {
        4
    } else if e.is::<io::Error>() {
        3
    } else {
        1
    }
}
