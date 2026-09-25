use std::collections::{BTreeMap, BTreeSet};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Flavor {
    Huawei,
    HuaweiPlus,
    OpenPlus,
    Open,
}

impl Flavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Huawei => "huawei",
            Self::HuaweiPlus => "huawei-plus",
            Self::OpenPlus => "open-plus",
            Self::Open => "open",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
pub enum System {
    Gps,
    Glonass,
    Galileo,
    Bds,
    Qzs,
}

impl System {
    pub const ALL: [Self; 5] = [
        Self::Gps,
        Self::Glonass,
        Self::Galileo,
        Self::Bds,
        Self::Qzs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Gps => "GPS",
            Self::Glonass => "GLONASS",
            Self::Galileo => "GALILEO",
            Self::Bds => "BDS",
            Self::Qzs => "QZS",
        }
    }

    pub fn code(self) -> char {
        match self {
            Self::Gps => 'G',
            Self::Glonass => 'R',
            Self::Galileo => 'E',
            Self::Bds => 'C',
            Self::Qzs => 'J',
        }
    }

    pub fn from_code(code: char) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.code() == code)
    }

    pub fn seed_key(self) -> &'static str {
        match self {
            Self::Gps => "gpsNav",
            Self::Glonass => "gloNav",
            Self::Galileo => "galNav",
            Self::Bds => "bdsNav",
            Self::Qzs => "qzsNav",
        }
    }

    pub fn record_size(self) -> usize {
        match self {
            Self::Gps => 80,
            Self::Glonass => 52,
            Self::Galileo | Self::Qzs => 76,
            Self::Bds => 92,
        }
    }

    pub fn slots(self) -> usize {
        match self {
            Self::Gps => 32,
            Self::Glonass => 24,
            Self::Galileo => 36,
            Self::Bds => 63,
            Self::Qzs => 10,
        }
    }

    pub fn subblocks(self) -> usize {
        if self == Self::Glonass { 8 } else { 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Seed,
    Agnss,
    Prediction,
    BdsPrediction,
    QzsPrediction,
    Broadcast,
    Antex,
    GpsAlmanac,
    GalileoAlmanac,
    QzsAlmanac,
}

#[derive(Clone, Debug, Serialize)]
pub struct Choice {
    pub orbit: &'static str,
    pub clock: &'static str,
    pub beyond_horizon: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub flavor: Flavor,
    pub products: BTreeMap<String, Choice>,
    pub sources: BTreeSet<Role>,
    pub epoch_count: usize,
    pub epoch_step_seconds: i64,
    pub degraded_permission_required: bool,
    pub fallback_chains: BTreeMap<&'static str, Vec<&'static str>>,
}

impl Plan {
    pub fn new(flavor: Flavor, systems: &[System], agnss: bool) -> Self {
        let mut products = BTreeMap::new();
        let mut sources = BTreeSet::from([Role::Broadcast]);
        let mut add = |name: String,
                       provider: &'static str,
                       clock: &'static str,
                       beyond_horizon: Option<&'static str>| {
            for source in [Some(provider), Some(clock), beyond_horizon]
                .into_iter()
                .flatten()
            {
                let role = match source {
                    "hiee" => Some(Role::Seed),
                    "hw" => Some(Role::Agnss),
                    "code5d" => Some(Role::Prediction),
                    "wum-nrt" => Some(Role::BdsPrediction),
                    "qzu" => Some(Role::QzsPrediction),
                    "brdc" => Some(Role::Broadcast),
                    _ => None,
                };
                if let Some(role) = role {
                    sources.insert(role);
                }
                if matches!(source, "code5d" | "wum-nrt" | "qzu") {
                    sources.insert(Role::Antex);
                }
            }
            products.insert(
                name,
                Choice {
                    orbit: provider,
                    clock,
                    beyond_horizon,
                },
            );
        };
        for &system in systems {
            let (provider, clock, tail) = match (flavor, system) {
                (Flavor::Huawei, _) => ("hiee", "hiee", None),
                (Flavor::HuaweiPlus, System::Qzs) => ("hiee", "hiee", None),
                (Flavor::HuaweiPlus, System::Glonass) => ("code5d", "hiee", None),
                (_, System::Bds) => (
                    "wum-nrt",
                    "wum-nrt",
                    Some(if flavor == Flavor::Open {
                        "empty"
                    } else {
                        "hiee"
                    }),
                ),
                (_, System::Qzs) => (
                    "qzu",
                    "qzu",
                    Some(if flavor == Flavor::Open {
                        "empty"
                    } else {
                        "hiee"
                    }),
                ),
                _ => ("code5d", "code5d", None),
            };
            add(format!("HW_PGNSS_{}", system.name()), provider, clock, tail);
        }
        if agnss {
            let provider = if matches!(flavor, Flavor::Huawei | Flavor::HuaweiPlus) {
                "hw"
            } else {
                "brdc"
            };
            add("HW_AGNSS_RTCM_33".into(), provider, provider, None);
        }
        let extra = if flavor == Flavor::Open {
            "open-almanacs-partial"
        } else {
            "hiee"
        };
        add("HW_PGNSS_EXTRA".into(), extra, extra, None);
        if flavor == Flavor::Open {
            sources.extend([Role::GpsAlmanac, Role::GalileoAlmanac]);
        }
        Self {
            flavor,
            products,
            sources,
            epoch_count: 36,
            epoch_step_seconds: 7200,
            degraded_permission_required: flavor == Flavor::Open,
            fallback_chains: BTreeMap::from([
                (
                    "code5d",
                    vec![
                        "code5d", "code-ult", "igs-ult", "gfz-ult", "wum-nrt", "brdc",
                    ],
                ),
                (
                    "wum-nrt",
                    if flavor == Flavor::Open {
                        vec!["wum-nrt", "empty"]
                    } else {
                        vec!["wum-nrt", "hiee"]
                    },
                ),
                (
                    "qzu",
                    if flavor == Flavor::Open {
                        vec!["qzu", "empty"]
                    } else {
                        vec!["qzu", "hiee"]
                    },
                ),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors_select_orbit_and_clock_independently() {
        let h = Plan::new(Flavor::HuaweiPlus, &System::ALL, true);
        let o = Plan::new(Flavor::OpenPlus, &System::ALL, true);
        assert_eq!(h.products["HW_PGNSS_GLONASS"].clock, "hiee");
        assert_eq!(o.products["HW_PGNSS_GLONASS"].clock, "code5d");
        assert_eq!(h.products["HW_PGNSS_QZS"].orbit, "hiee");
        assert_eq!(o.products["HW_PGNSS_QZS"].orbit, "qzu");
        assert_eq!(o.products["HW_PGNSS_QZS"].beyond_horizon, Some("hiee"));
        let open = Plan::new(Flavor::Open, &System::ALL, true);
        assert!(!open.sources.contains(&Role::Seed));
        assert!(!open.sources.contains(&Role::Agnss));
        assert!(open.degraded_permission_required);
        let huawei = Plan::new(Flavor::Huawei, &[System::Gps], false);
        assert_eq!(
            huawei.sources,
            BTreeSet::from([Role::Seed, Role::Broadcast])
        );
    }
}
