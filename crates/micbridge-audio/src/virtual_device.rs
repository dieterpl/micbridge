//! Working out which playback device feeds which microphone.
//!
//! A virtual audio cable is two devices that are internally joined: a playback half
//! and a recording half. To make audio appear on a microphone, you render into the
//! playback half; the game then selects the recording half.
//!
//! That is easy to get backwards, and the consequence is silence from a setup that
//! otherwise looks completely healthy — or worse, the audio simply coming out of the
//! speakers. So the useful question is not "which output device shall I render into"
//! but **"which microphone should the game hear"**, with the playback half derived
//! from it. That is what this module answers.
//!
//! The pairing is structural, not a list of product names: it works from the two
//! device lists the machine actually reports, so a cable this code has never heard of
//! pairs correctly as long as it follows one of the two naming conventions in use.

use anyhow::Result;

use crate::devices::{list_devices, Direction};

/// A way to make audio appear on a microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicRoute {
    /// The recording device a game selects as its microphone.
    pub game_mic: String,
    /// The playback device to render into so audio reaches `game_mic`.
    pub render_into: String,
    /// How the two were matched. Affects how much to trust the pairing.
    pub how: Pairing,
}

/// How a microphone was matched to a playback device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// The names differ only by Output/Input, e.g. "X Output" fed by "X Input".
    ///
    /// Strong evidence of a cable: real hardware is rarely named as such a pair, and
    /// the two halves are explicitly describing the ends of one route.
    NamedPair,
    /// One device carries the same name in both directions.
    ///
    /// Weaker. Some virtual devices are built this way, but **so is every duplex
    /// audio interface** — a USB interface appears as both an input and an output
    /// without being a loopback at all. Rendering into one of those sends audio to
    /// its physical outputs, not to its input, so this pairing is offered but not
    /// preferred.
    SameName,
}

impl Pairing {
    /// True when the pairing is confident enough to choose without being asked.
    pub fn is_reliable(self) -> bool {
        matches!(self, Self::NamedPair)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::NamedPair => "matched as an Input/Output pair",
            Self::SameName => {
                "same name in both directions — may be a duplex interface rather than a cable"
            }
        }
    }
}

/// Swaps the first case-insensitive occurrence of `from` for `to`.
///
/// Only the first, so "X Output (Some Output Device)" becomes
/// "X Input (Some Output Device)" without corrupting the trailing product name.
fn swap_first(name: &str, from: &str, to: &str) -> Option<String> {
    let at = name.to_lowercase().find(&from.to_lowercase())?;
    let mut swapped = String::with_capacity(name.len() + to.len());
    swapped.push_str(&name[..at]);
    swapped.push_str(to);
    swapped.push_str(&name[at + from.len()..]);
    Some(swapped)
}

/// Finds the device in `haystack` whose name matches `wanted`.
///
/// Falls back to comparing only the part before a parenthesised suffix, because
/// Windows does not always decorate the two halves of a cable identically.
fn find_device_named(wanted: &str, haystack: &[String]) -> Option<String> {
    if let Some(found) = haystack.iter().find(|name| name.eq_ignore_ascii_case(wanted)) {
        return Some(found.clone());
    }
    let prefix = wanted.split(" (").next().unwrap_or(wanted).trim().to_lowercase();
    if prefix.is_empty() {
        return None;
    }
    haystack.iter().find(|name| name.to_lowercase().starts_with(&prefix)).cloned()
}

/// Every way of getting audio onto a microphone on this machine.
///
/// Reliable pairings come first, so the caller can take the head of the list as a
/// default without having to reason about it.
pub fn routes(inputs: &[String], outputs: &[String]) -> Vec<MicRoute> {
    let mut routes = Vec::new();

    for game_mic in inputs {
        // The strong rule: "X Output" is fed by "X Input".
        let paired = swap_first(game_mic, "output", "Input")
            .filter(|candidate| !candidate.eq_ignore_ascii_case(game_mic))
            .and_then(|candidate| find_device_named(&candidate, outputs));

        if let Some(render_into) = paired {
            routes.push(MicRoute {
                game_mic: game_mic.clone(),
                render_into,
                how: Pairing::NamedPair,
            });
            continue;
        }

        // The weak rule: the same name in both directions.
        if let Some(render_into) = outputs.iter().find(|name| name.eq_ignore_ascii_case(game_mic)) {
            routes.push(MicRoute {
                game_mic: game_mic.clone(),
                render_into: render_into.clone(),
                how: Pairing::SameName,
            });
        }
    }

    routes.sort_by_key(|route| !route.how.is_reliable());
    routes
}

/// [`routes`] against the machine's real device lists.
pub fn detect() -> Result<Vec<MicRoute>> {
    let inputs = list_devices(Direction::Input)?;
    let outputs = list_devices(Direction::Output)?;
    Ok(routes(&inputs, &outputs))
}

/// The route whose playback half is `render_into`, if any.
///
/// Used to answer "given that I am rendering here, which microphone will the game
/// need to select".
pub fn route_for_render_device(render_into: &str, routes: &[MicRoute]) -> Option<MicRoute> {
    routes.iter().find(|route| route.render_into.eq_ignore_ascii_case(render_into)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names as Windows reports them with a cable installed alongside real hardware.
    fn windows_with_a_cable() -> (Vec<String>, Vec<String>) {
        let inputs = vec![
            "Microphone (Realtek(R) Audio)".to_string(),
            "CABLE Output (VB-Audio Virtual Cable)".to_string(),
        ];
        let outputs = vec![
            "Speakers (Realtek(R) Audio)".to_string(),
            "CABLE Input (VB-Audio Virtual Cable)".to_string(),
        ];
        (inputs, outputs)
    }

    #[test]
    fn finds_the_playback_half_that_feeds_a_microphone() {
        // The whole point: the user names a microphone, and the playback device to
        // render into is derived — never the other way round.
        let (inputs, outputs) = windows_with_a_cable();
        let routes = routes(&inputs, &outputs);

        assert_eq!(routes.len(), 1, "one cable, one route: {routes:?}");
        assert_eq!(routes[0].game_mic, "CABLE Output (VB-Audio Virtual Cable)");
        assert_eq!(routes[0].render_into, "CABLE Input (VB-Audio Virtual Cable)");
        assert_eq!(routes[0].how, Pairing::NamedPair);
    }

    #[test]
    fn real_speakers_and_microphones_are_not_a_route() {
        // Rendering into real speakers would put the audio in the room and leave the
        // game with no microphone at all — the failure this module exists to prevent.
        let (inputs, outputs) = windows_with_a_cable();
        let routes = routes(&inputs, &outputs);
        assert!(
            !routes.iter().any(|r| r.render_into.contains("Speakers")),
            "must not offer to render into real speakers: {routes:?}"
        );
    }

    #[test]
    fn no_product_names_are_required() {
        // Entirely invented names, following the Input/Output convention.
        let inputs = vec!["Widget Output".to_string()];
        let outputs = vec!["Widget Input".to_string()];
        let routes = routes(&inputs, &outputs);
        assert_eq!(routes.len(), 1, "pairing must be structural, not a vendor list");
        assert_eq!(routes[0].render_into, "Widget Input");
        assert_eq!(routes[0].how, Pairing::NamedPair);
    }

    #[test]
    fn a_duplex_interface_is_offered_only_as_a_weak_pairing() {
        // A USB interface appears in both directions without being a loopback.
        // Reporting it as reliable would send audio to its physical outputs.
        let inputs = vec!["UMC204HD 192k".to_string()];
        let outputs = vec!["UMC204HD 192k".to_string()];
        let routes = routes(&inputs, &outputs);

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].how, Pairing::SameName);
        assert!(!routes[0].how.is_reliable(), "a duplex interface is not a known-good route");
    }

    #[test]
    fn reliable_routes_are_listed_first() {
        // So a caller can take the head of the list as a default.
        let inputs = vec!["UMC204HD 192k".to_string(), "Widget Output".to_string()];
        let outputs = vec!["UMC204HD 192k".to_string(), "Widget Input".to_string()];
        let routes = routes(&inputs, &outputs);

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].game_mic, "Widget Output", "the named pair should sort first");
        assert!(routes[0].how.is_reliable());
        assert!(!routes[1].how.is_reliable());
    }

    #[test]
    fn a_machine_with_no_cable_offers_nothing() {
        let inputs = vec!["Microphone (Realtek(R) Audio)".to_string()];
        let outputs = vec!["Speakers (Realtek(R) Audio)".to_string()];
        assert!(routes(&inputs, &outputs).is_empty());
    }

    #[test]
    fn the_trailing_suffix_is_not_rewritten() {
        let swapped = swap_first("CABLE Output (Vendor Output Device)", "output", "Input")
            .expect("contains output");
        assert_eq!(swapped, "CABLE Input (Vendor Output Device)");
    }

    #[test]
    fn matching_ignores_case() {
        let inputs = vec!["cable output (vendor)".to_string()];
        let outputs = vec!["CABLE INPUT (Vendor)".to_string()];
        assert_eq!(routes(&inputs, &outputs).len(), 1, "case should not decide this");
    }

    #[test]
    fn pairs_when_only_the_leading_name_matches() {
        // Windows does not always decorate both halves identically.
        let inputs = vec!["CABLE Output (VB-Audio Virtual Cable)".to_string()];
        let outputs = vec!["CABLE Input (VB-Audio Point)".to_string()];
        let routes = routes(&inputs, &outputs);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].render_into, "CABLE Input (VB-Audio Point)");
    }

    #[test]
    fn a_render_device_can_be_traced_back_to_its_microphone() {
        let (inputs, outputs) = windows_with_a_cable();
        let all = routes(&inputs, &outputs);

        let found = route_for_render_device("CABLE Input (VB-Audio Virtual Cable)", &all)
            .expect("should trace back");
        assert_eq!(found.game_mic, "CABLE Output (VB-Audio Virtual Cable)");

        assert!(
            route_for_render_device("Speakers (Realtek(R) Audio)", &all).is_none(),
            "real speakers feed no microphone"
        );
    }
}
