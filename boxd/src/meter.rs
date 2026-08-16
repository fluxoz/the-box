//! The meter: what this Box costs to run, next to what the same stack rents
//! for in a cloud. The whole business thesis, rendered as a number the owner
//! glances at — and every number here is an ESTIMATE and says so. Honest
//! ballparks beat precise-looking fiction: hardware classes carry a typical
//! wall-power figure with its assumption stated, and cloud equivalents use
//! deliberately conservative list-price anchors, itemized so the reader can
//! argue with any line.

use serde::Serialize;

use crate::config::BoxConfig;
use crate::paths::Paths;

/// US average residential electricity, USD per kWh, as the default. The owner
/// can set their real rate on the System page; we never pretend to know it.
pub const DEFAULT_RATE_PER_KWH: f64 = 0.17;

#[derive(Debug, Clone, Serialize)]
pub struct PowerEstimate {
    pub watts: f64,
    /// The assumption behind the wattage, stated for the reader.
    pub basis: String,
}

/// Typical wall power for this Box's hardware class. Classes, not precision:
/// a Pi idles in single digits, a mini-PC in the teens, and an always-on
/// machine with a discrete NVIDIA card carries its idle draw all day.
pub fn power_estimate(board: Option<&str>, gpu: Option<&str>) -> PowerEstimate {
    match (board, gpu) {
        (Some(b @ ("pi3" | "pi4")), _) => PowerEstimate {
            watts: 5.0,
            basis: format!("a Raspberry Pi {} under light load", &b[2..]),
        },
        (Some("pi5"), _) => PowerEstimate {
            watts: 8.0,
            basis: "a Raspberry Pi 5 under light load".into(),
        },
        (_, Some("nvidia")) => PowerEstimate {
            watts: 60.0,
            basis: "a desktop with a discrete NVIDIA card, mostly idle (bursts cost more)".into(),
        },
        _ => PowerEstimate {
            watts: 15.0,
            basis: "a small x86 machine (mini-PC / old laptop) mostly idle".into(),
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudLine {
    pub what: String,
    pub monthly_usd: f64,
    /// What this line is anchored to, so the reader can argue with it.
    pub anchor: String,
}

/// What renting this Box's declared stack would plausibly cost, itemized.
/// Anchors are conservative list-price ballparks for comparable managed
/// products, not worst cases.
pub fn cloud_equivalent(config: &BoxConfig, gpu: Option<&str>) -> Vec<CloudLine> {
    let mut lines = vec![CloudLine {
        what: "a small always-on cloud server".into(),
        monthly_usd: 12.0,
        anchor: "2 vCPU / 4 GB VPS, typical list price".into(),
    }];
    for s in &config.services {
        match s.template.as_str() {
            "container" => {
                let image = s
                    .params
                    .get("image")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let (what, monthly, anchor): (String, f64, &str) =
                    if image.contains("postgres") || image.contains("redis") {
                        (
                            format!("managed database ({})", s.name),
                            15.0,
                            "entry managed Postgres/Redis tier",
                        )
                    } else if image.contains("minio") {
                        (
                            format!("object storage ({})", s.name),
                            5.0,
                            "~100 GB of managed object storage",
                        )
                    } else if image.contains("llama") || image.contains("open-webui") {
                        (
                            format!("hosted AI inference ({})", s.name),
                            20.0,
                            "light metered LLM API use; heavy use costs far more",
                        )
                    } else {
                        (
                            format!("hosted app ({})", s.name),
                            7.0,
                            "small managed container instance",
                        )
                    };
                lines.push(CloudLine {
                    what,
                    monthly_usd: monthly,
                    anchor: anchor.into(),
                });
            }
            "reverse-proxied-app" => lines.push(CloudLine {
                what: format!("hosted app ({})", s.name),
                monthly_usd: 7.0,
                anchor: "small managed app instance".into(),
            }),
            // Static sites: free tiers exist and honesty beats padding.
            _ => {}
        }
    }
    if gpu == Some("nvidia") {
        lines.push(CloudLine {
            what: "a cloud GPU when you need one".into(),
            monthly_usd: 90.0,
            anchor: "~3 hours/day of an entry cloud GPU instance".into(),
        });
    }
    if config.backup.as_ref().is_some_and(|b| b.enabled) {
        lines.push(CloudLine {
            what: "managed backups".into(),
            monthly_usd: 3.0,
            anchor: "snapshot storage for a small server".into(),
        });
    }
    lines
}

#[derive(Debug, Clone, Serialize)]
pub struct Meter {
    pub power: PowerEstimate,
    pub rate_per_kwh: f64,
    pub electricity_monthly_usd: f64,
    pub cloud_lines: Vec<CloudLine>,
    pub cloud_monthly_usd: f64,
    pub note: &'static str,
}

/// The whole card. `rate` falls back to the stated US-average default.
pub fn read(paths: &Paths, config: &BoxConfig, rate: Option<f64>) -> Meter {
    let (board, gpu) = crate::channel::ChannelConfig::load(paths)
        .ok()
        .flatten()
        .map(|c| (c.board, c.gpu))
        .unwrap_or((None, None));
    let power = power_estimate(board.as_deref(), gpu.as_deref());
    let rate = rate.unwrap_or(DEFAULT_RATE_PER_KWH);
    let electricity = power.watts * 24.0 * 30.0 / 1000.0 * rate;
    let cloud_lines = cloud_equivalent(config, gpu.as_deref());
    let cloud_total: f64 = cloud_lines.iter().map(|l| l.monthly_usd).sum();
    Meter {
        power,
        rate_per_kwh: rate,
        electricity_monthly_usd: (electricity * 100.0).round() / 100.0,
        cloud_lines,
        cloud_monthly_usd: cloud_total,
        note: "Estimates on both sides, and they say what they assume. \
               The point is the ratio, not the pennies.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_math_stay_honest() {
        // A Pi's month of power is a rounding error; the basis says why.
        let pi = power_estimate(Some("pi5"), None);
        assert_eq!(pi.watts, 8.0);
        assert!(pi.basis.contains("Pi 5"));
        // 8 W * 720 h = 5.76 kWh * $0.17 ≈ $0.98.
        let kwh = pi.watts * 24.0 * 30.0 / 1000.0;
        assert!((kwh * DEFAULT_RATE_PER_KWH - 0.98).abs() < 0.01);

        // A GPU box carries its idle draw and its cloud counterpart is priced.
        let gpu = power_estimate(None, Some("nvidia"));
        assert!(gpu.watts > 15.0);
        let config = BoxConfig::default();
        let lines = cloud_equivalent(&config, Some("nvidia"));
        assert!(lines.iter().any(|l| l.what.contains("GPU")));

        // Static sites do not pad the cloud bill: free tiers exist.
        assert_eq!(cloud_equivalent(&BoxConfig::default(), None).len(), 1);
    }
}
