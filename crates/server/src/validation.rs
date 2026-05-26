//! Server-side validation of fields coming from untrusted clients.
//!
//! The proto wire format is permissive — strings can be up to ~4 MB,
//! contain any byte, and (for `frequency`) any value at all. The
//! shipped client constrains itself to sensible inputs at the GUI
//! layer, but a hand-crafted or hostile client can send anything.
//!
//! These helpers enforce the same invariants the client's GUI does,
//! server-side, so a malformed payload is rejected with a clean
//! `INVALID_ARGUMENT` rather than allowed to bloat the registry
//! (room HashMap squat), corrupt operator logs (ANSI/control-char
//! injection), or partition a populated room across not-quite-equal
//! frequency strings ("446.05" vs "446.050").

use tonic::Status;

/// Hard cap on a client's display name (bytes, not UTF-8 chars). The
/// shipped GUI caps at 10 chars + uppercases; the server is more
/// permissive (32 bytes) so future clients can carry slightly richer
/// identifiers without requiring a server bump. Anything longer is
/// almost certainly an attack or a bug.
const MAX_DISPLAY_NAME_LEN: usize = 32;

/// Maximum byte length of an incoming `frequency` string. Any
/// legitimate value renders to ≤ 6 chars ("446.05"); 12 leaves room
/// for sign/exponent oddities while still bounding HashMap key size.
const MAX_FREQUENCY_LEN: usize = 12;

// PMR-adjacent band Toki uses. Mirrors the client's `theme.rs`
// constants — duplicated here rather than imported because the
// server doesn't depend on the client crate, and the values are
// genuinely protocol-level (the *server* defines what a valid
// frequency is; the client just has to agree).
const FREQ_MIN_MHZ: f32 = 446.00;
const FREQ_MAX_MHZ: f32 = 448.00;
const FREQ_STEP_MHZ: f32 = 0.05;
/// Tolerance for "step-aligned" — single-precision float math means
/// an exact equality check fails for some bit patterns that round to
/// the same channel.
const FREQ_STEP_TOLERANCE: f32 = 0.001;

/// Validate a freshly-registered client's display name. Returns the
/// trimmed name on success.
///
/// Rejects:
///   * Empty (post-trim) — nameless clients are useless to identify
///     and confuse the operator's logs.
///   * Longer than `MAX_DISPLAY_NAME_LEN` bytes — bounds memory and
///     log line length; the legitimate client caps at 10 chars.
///   * Any control character (byte `< 0x20` or `== 0x7F`) — prevents
///     ANSI-escape log injection via `tracing::info!(name = %…)`.
pub fn display_name(raw: &str) -> Result<String, Status> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("display_name is required"));
    }
    if trimmed.len() > MAX_DISPLAY_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "display_name exceeds {MAX_DISPLAY_NAME_LEN} bytes"
        )));
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(Status::invalid_argument(
            "display_name contains control characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Validate a frequency string from `join` / `change_frequency`.
/// Returns the *canonical* form (e.g. `"446.05"`) on success —
/// callers should use this value as the room HashMap key so two
/// clients writing `"446.05"` and `"446.050"` end up in the same
/// room rather than two singletons.
///
/// Rejects:
///   * Empty / too long.
///   * Not parseable as `f32`.
///   * Out of the `[FREQ_MIN_MHZ, FREQ_MAX_MHZ]` band.
///   * Not step-aligned to `FREQ_STEP_MHZ` within tolerance.
pub fn frequency(raw: &str) -> Result<String, Status> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("frequency is required"));
    }
    if trimmed.len() > MAX_FREQUENCY_LEN {
        return Err(Status::invalid_argument("frequency string too long"));
    }
    let f: f32 = trimmed.parse().map_err(|_| {
        Status::invalid_argument(format!("frequency {trimmed:?} is not a number"))
    })?;
    if !f.is_finite() {
        return Err(Status::invalid_argument("frequency must be finite"));
    }
    if !(FREQ_MIN_MHZ..=FREQ_MAX_MHZ).contains(&f) {
        return Err(Status::invalid_argument(format!(
            "frequency {f} out of band [{FREQ_MIN_MHZ}, {FREQ_MAX_MHZ}]"
        )));
    }
    let steps = (f - FREQ_MIN_MHZ) / FREQ_STEP_MHZ;
    let nearest_step = steps.round();
    if (steps - nearest_step).abs() > FREQ_STEP_TOLERANCE {
        return Err(Status::invalid_argument(format!(
            "frequency {f} not step-aligned to {FREQ_STEP_MHZ} MHz"
        )));
    }
    let canonical = FREQ_MIN_MHZ + nearest_step * FREQ_STEP_MHZ;
    Ok(format!("{canonical:.2}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_accepts_normal() {
        assert_eq!(display_name("anon").unwrap(), "anon");
        assert_eq!(display_name("  TOKI-1  ").unwrap(), "TOKI-1");
        assert_eq!(display_name("Über").unwrap(), "Über");
    }

    #[test]
    fn display_name_rejects_empty_and_whitespace() {
        assert!(display_name("").is_err());
        assert!(display_name("   ").is_err());
    }

    #[test]
    fn display_name_rejects_too_long() {
        let s: String = "x".repeat(MAX_DISPLAY_NAME_LEN + 1);
        assert!(display_name(&s).is_err());
        let edge: String = "x".repeat(MAX_DISPLAY_NAME_LEN);
        assert!(display_name(&edge).is_ok());
    }

    #[test]
    fn display_name_rejects_control_chars() {
        // Newlines / CR / tab — the obvious log-injection payloads.
        assert!(display_name("a\nb").is_err());
        assert!(display_name("a\rb").is_err());
        assert!(display_name("a\tb").is_err());
        // ANSI escape sequence introducer.
        assert!(display_name("\x1b[31mEVIL\x1b[0m").is_err());
        // DEL.
        assert!(display_name("a\x7fb").is_err());
    }

    #[test]
    fn frequency_accepts_band_boundaries_and_mid() {
        assert_eq!(frequency("446.00").unwrap(), "446.00");
        assert_eq!(frequency("447.00").unwrap(), "447.00");
        assert_eq!(frequency("448.00").unwrap(), "448.00");
    }

    #[test]
    fn frequency_canonicalises_equivalent_forms() {
        // "446.050", "446.05", " 446.05 " should all canonicalise.
        assert_eq!(frequency("446.05").unwrap(), "446.05");
        assert_eq!(frequency("446.050").unwrap(), "446.05");
        assert_eq!(frequency(" 446.05 ").unwrap(), "446.05");
    }

    #[test]
    fn frequency_rejects_out_of_band() {
        assert!(frequency("445.99").is_err());
        assert!(frequency("448.01").is_err());
        assert!(frequency("0").is_err());
        assert!(frequency("999").is_err());
    }

    #[test]
    fn frequency_rejects_not_step_aligned() {
        // 0.01 MHz off step.
        assert!(frequency("446.01").is_err());
        assert!(frequency("447.07").is_err());
    }

    #[test]
    fn frequency_rejects_non_numeric_and_weird() {
        assert!(frequency("").is_err());
        assert!(frequency("abc").is_err());
        assert!(frequency("' OR 1=1").is_err());
        assert!(frequency("NaN").is_err()); // parses to NaN, must be filtered.
        assert!(frequency("inf").is_err());
        // Excessively long inputs are rejected before parse.
        let s: String = "4".repeat(100);
        assert!(frequency(&s).is_err());
    }
}
