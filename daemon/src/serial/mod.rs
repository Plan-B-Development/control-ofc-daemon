//! OpenFanController serial protocol and transport, and the id the rest of the
//! daemon uses to name one of its channels.

pub mod adoption;
pub mod controller;
pub mod protocol;
pub mod real_transport;
pub mod transport;

// ── The `openfan:ch{NN}` member id (`P8-bq`) ─────────────────────────
//
// This is the API-facing name for an OpenFan channel: it appears in profiles,
// in cooling-device configs, in `/fans` entries and in every `PwmCommand` the
// engine evaluates. It is NOT the serial wire format — that is `protocol.rs`,
// which is why this lives at the subsystem root rather than beside
// `NUM_CHANNELS`.
//
// One producer and one parser, because it had six: three inline `format!`s and
// three inline `strip_prefix(...).parse()`s, two of the latter on the
// single-writer path. They all agreed, which is exactly the condition under
// which a seventh gets written slightly differently.

/// The member-id prefix for an OpenFanController channel.
pub const OPENFAN_MEMBER_PREFIX: &str = "openfan:ch";

/// Build the member id for an OpenFan channel.
///
/// The zero-padded two-digit form is **persisted** — it is written into saved
/// profiles and cooling-device configs — so this formatting is a compatibility
/// surface, not presentation. `openfan_member_id_is_zero_padded_and_stable`
/// pins it.
pub fn openfan_member_id(channel: u8) -> String {
    format!("{OPENFAN_MEMBER_PREFIX}{channel:02}")
}

/// Why a member id does not name an OpenFan channel.
///
/// Two variants rather than an `Option` because the profile engine logs the two
/// cases **differently** on the single-writer path — "malformed member_id" for
/// an id that is not ours at all, "unparseable channel" for one that is ours and
/// carries a broken suffix. Collapsing them would lose an operator's only signal
/// for which kind of bad id reached the engine. Call sites that do not need the
/// distinction use `.ok()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFanMemberIdError {
    /// The id does not carry the OpenFan prefix.
    NotOpenFan,
    /// It carries the prefix, but the channel suffix will not parse.
    UnparseableChannel,
}

/// Parse the channel out of an OpenFan member id.
///
/// Deliberately as permissive as the six inline copies it replaces: `u8::from_str`
/// accepts `+5` and `003`, so `openfan:ch+5` and `openfan:ch003` both resolve to
/// channels 5 and 3 as they always have. Tightening that would be a behaviour
/// change on the single-writer path and is not in this change's scope.
pub fn openfan_channel_of(member_id: &str) -> Result<u8, OpenFanMemberIdError> {
    let suffix = member_id
        .strip_prefix(OPENFAN_MEMBER_PREFIX)
        .ok_or(OpenFanMemberIdError::NotOpenFan)?;
    suffix
        .parse::<u8>()
        .map_err(|_| OpenFanMemberIdError::UnparseableChannel)
}

#[cfg(test)]
mod openfan_member_id_tests {
    use super::*;

    #[test]
    fn the_id_round_trips_for_every_channel_the_controller_has() {
        // What this checks is producer/parser AGREEMENT over the real channel
        // range — not the id's shape. It cannot catch a prefix or padding drift
        // and it would be dishonest to imply otherwise: both halves resolve the
        // same `OPENFAN_MEMBER_PREFIX`, and `NUM_CHANNELS` is 10, so every id
        // here is single-digit and round-trips with or without the `:02`. The
        // persisted shape is pinned by
        // `openfan_member_id_is_zero_padded_and_stable`, which is the test to
        // read before touching either constant.
        for ch in 0..protocol::NUM_CHANNELS {
            let id = openfan_member_id(ch);
            assert_eq!(
                openfan_channel_of(&id),
                Ok(ch),
                "round trip failed for channel {ch} (id {id:?})"
            );
        }
    }

    #[test]
    fn openfan_member_id_is_zero_padded_and_stable() {
        // This exact string is PERSISTED — saved profiles and cooling-device
        // configs carry it — so the padding is a compatibility surface. Pinned
        // as literals on purpose: a relationship here would just restate the
        // implementation and could not catch the padding being dropped.
        assert_eq!(openfan_member_id(0), "openfan:ch00");
        assert_eq!(openfan_member_id(3), "openfan:ch03");
        assert_eq!(openfan_member_id(9), "openfan:ch09");
        // Above the controller's channel count the format still holds; nothing
        // clamps here, and a caller that invents a channel gets a truthful id
        // rather than a silently reshaped one.
        assert_eq!(openfan_member_id(10), "openfan:ch10");
    }

    #[test]
    fn the_two_failure_modes_stay_distinguishable() {
        // The engine logs these differently on the single-writer path, so the
        // distinction is contract, not convenience.
        assert_eq!(
            openfan_channel_of("hwmon:it8696:isa-0a40:pwm5:PUMP"),
            Err(OpenFanMemberIdError::NotOpenFan)
        );
        assert_eq!(
            openfan_channel_of(""),
            Err(OpenFanMemberIdError::NotOpenFan)
        );
        assert_eq!(
            openfan_channel_of("openfan:chXX"),
            Err(OpenFanMemberIdError::UnparseableChannel)
        );
        assert_eq!(
            openfan_channel_of("openfan:ch"),
            Err(OpenFanMemberIdError::UnparseableChannel),
            "the prefix alone is ours-but-broken, not someone else's id"
        );
        assert_eq!(
            openfan_channel_of("openfan:ch999"),
            Err(OpenFanMemberIdError::UnparseableChannel),
            "999 overflows u8 — ours, and broken"
        );
    }

    #[test]
    fn the_permissive_forms_the_inline_copies_accepted_are_still_accepted() {
        // `u8::from_str` takes `+5` and `003`, so the six inline copies did too.
        // Centralising must not silently tighten what the single-writer path
        // accepts — that would be a behaviour change wearing a refactor's
        // clothes. If this is ever deliberately narrowed, it is its own change.
        assert_eq!(openfan_channel_of("openfan:ch+5"), Ok(5));
        assert_eq!(openfan_channel_of("openfan:ch003"), Ok(3));
        assert_eq!(openfan_channel_of("openfan:ch5"), Ok(5));
    }
}
