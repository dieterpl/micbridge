//! Device enumeration and lookup.
//!
//! Matching is by name substring rather than by index. Indices shift when a USB
//! device is unplugged, and the two names that matter here — "UMC204HD" on the
//! Mac and "CABLE Input" on Windows — are stable and easy to type.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// Names of every device in the given direction, skipping any whose name cannot
/// be read.
///
/// A device that fails `name()` is a device we could not address anyway, so it
/// is omitted rather than reported as an error that would block listing the
/// rest.
pub fn list_devices(direction: Direction) -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = match direction {
        Direction::Input => host.input_devices().context("enumerating input devices")?.collect(),
        Direction::Output => host.output_devices().context("enumerating output devices")?.collect(),
    };
    Ok(devices.iter().filter_map(|d| d.name().ok()).collect())
}

/// Finds a device by case-insensitive substring, or takes the default when
/// `wanted` is `None`.
pub fn find_device(direction: Direction, wanted: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();

    let Some(wanted) = wanted else {
        let device = match direction {
            Direction::Input => host.default_input_device(),
            Direction::Output => host.default_output_device(),
        };
        return device.ok_or_else(|| anyhow!("no default {} device", direction.label()));
    };

    let needle = wanted.to_lowercase();
    let devices: Vec<cpal::Device> = match direction {
        Direction::Input => host.input_devices().context("enumerating input devices")?.collect(),
        Direction::Output => host.output_devices().context("enumerating output devices")?.collect(),
    };

    for device in &devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&needle) {
                return Ok(device.clone());
            }
        }
    }

    // Listing what *is* available turns the most common setup mistake — a typo,
    // or VB-CABLE not installed yet — into a self-answering error.
    let available: Vec<String> = devices.iter().filter_map(|d| d.name().ok()).collect();
    Err(anyhow!(
        "no {} device matching {wanted:?}. Available: {}",
        direction.label(),
        if available.is_empty() { "none".to_string() } else { available.join(", ") }
    ))
}

/// A one-line summary of the current defaults, for the startup log.
pub fn describe_default_devices() -> String {
    let host = cpal::default_host();
    let input =
        host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_else(|| "none".into());
    let output =
        host.default_output_device().and_then(|d| d.name().ok()).unwrap_or_else(|| "none".into());
    format!("default input {input:?}, default output {output:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_devices_does_not_fail_on_a_machine_with_none() {
        // CI runners have no audio hardware. Enumeration must still succeed with
        // an empty list rather than erroring, or the test suite could not run
        // anywhere useful.
        for direction in [Direction::Input, Direction::Output] {
            let listed = list_devices(direction);
            assert!(listed.is_ok(), "{direction:?} enumeration failed: {listed:?}");
        }
    }

    #[test]
    fn a_missing_device_names_the_alternatives() {
        // `cpal::Device` has no `Debug`, so this cannot use `expect_err`.
        let message = match find_device(Direction::Output, Some("definitely-not-a-real-device")) {
            Ok(_) => panic!("a device with this name should not exist"),
            Err(err) => err.to_string(),
        };
        assert!(message.contains("Available:"), "error should list alternatives: {message}");
    }
}
