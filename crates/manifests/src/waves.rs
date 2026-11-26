//! `RESERVED_IDS` and `WAVES.toml` parsing and validation (M12).

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// The 12 seed manifest IDs (allowed only in `sources/seed/`).
pub const SEED_MANIFEST_IDS: [&str; 12] = [
    "arxiv",
    "crossref",
    "openaire",
    "europepmc",
    "openalex",
    "semanticscholar",
    "unpaywall",
    "pmc_idconv",
    "doaj",
    "datacite",
    "opencitations",
    "mwmbl",
];

/// The 3 legacy protocol instance IDs (allowed only in `instances/seed/`).
pub const LEGACY_INSTANCE_IDS: [&str; 3] = ["wikipedia", "wikimedia", "dataverse"];

/// Parsed `RESERVED_IDS` file (62 IDs: 12 seeds + 42 native + 3 legacy instances + 5 dropped).
#[derive(Debug, Clone)]
pub struct ReservedIds {
    pub all: BTreeSet<String>,
    /// Native engines (42) + dropped legacy engines (5) = 47 IDs forbidden in ALL manifests/instances.
    pub non_manifest_reserved: BTreeSet<String>,
}

impl ReservedIds {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut all = BTreeSet::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if !all.insert(line.to_owned()) {
                return Err(format!(
                    "RESERVED_IDS line {}: duplicate id {:?}",
                    lineno + 1,
                    line
                ));
            }
        }
        let seeds: BTreeSet<&str> = SEED_MANIFEST_IDS.into_iter().collect();
        let legacy_insts: BTreeSet<&str> = LEGACY_INSTANCE_IDS.into_iter().collect();
        let non_manifest_reserved: BTreeSet<String> = all
            .iter()
            .filter(|id| !seeds.contains(id.as_str()) && !legacy_insts.contains(id.as_str()))
            .cloned()
            .collect();
        Ok(Self {
            all,
            non_manifest_reserved,
        })
    }

    pub fn is_seed_manifest(id: &str) -> bool {
        SEED_MANIFEST_IDS.contains(&id)
    }

    pub fn is_legacy_instance(id: &str) -> bool {
        LEGACY_INSTANCE_IDS.contains(&id)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WavesFile {
    #[serde(default)]
    pub waves: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub m13_entities: BTreeMap<String, Vec<String>>,
}

/// Indexed `WAVES.toml` allocation.
#[derive(Debug, Clone)]
pub struct WaveAllocation {
    pub waves: BTreeMap<String, BTreeSet<String>>,
    pub id_to_wave: BTreeMap<String, String>,
    pub m13_entities: BTreeMap<String, Vec<String>>,
}

impl WaveAllocation {
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: WavesFile =
            toml::from_str(text).map_err(|e| format!("WAVES.toml parse error: {e}"))?;
        let mut waves = BTreeMap::new();
        let mut id_to_wave: BTreeMap<String, String> = BTreeMap::new();
        for (wave, ids) in raw.waves {
            let mut set = BTreeSet::new();
            for id in ids {
                if !set.insert(id.clone()) {
                    return Err(format!("WAVES.toml: duplicate id {id:?} in wave {wave:?}"));
                }
                if let Some(prev) = id_to_wave.insert(id.clone(), wave.clone()) {
                    return Err(format!(
                        "WAVES.toml: id {id:?} assigned to both {prev:?} and {wave:?}"
                    ));
                }
            }
            waves.insert(wave, set);
        }
        Ok(Self {
            waves,
            id_to_wave,
            m13_entities: raw.m13_entities,
        })
    }

    /// Validate that an ID in `sources/<wave>/` or `instances/<wave>/` belongs to `wave`
    /// and does not violate `RESERVED_IDS`.
    pub fn check_placement(
        &self,
        reserved: &ReservedIds,
        id: &str,
        wave: &str,
        is_instance: bool,
    ) -> Result<(), String> {
        if reserved.non_manifest_reserved.contains(id) {
            return Err(format!(
                "id {id:?} is reserved for a native/dropped engine in RESERVED_IDS"
            ));
        }
        if is_instance && ReservedIds::is_seed_manifest(id) {
            return Err(format!(
                "id {id:?} is reserved for a seed manifest (sources/seed/), not a protocol instance"
            ));
        }
        if !is_instance && ReservedIds::is_legacy_instance(id) {
            return Err(format!(
                "id {id:?} is reserved for a legacy protocol instance (instances/seed/), not a manifest"
            ));
        }
        if wave == "seed" {
            if is_instance {
                // Curated seed protocol instances (W2b/W2c) live in instances/seed/;
                // only reject if assigned to a w5* wave.
                if let Some(assigned) = self.id_to_wave.get(id)
                    && assigned != "seed"
                {
                    return Err(format!(
                        "id {id:?} is assigned to wave {assigned:?} in WAVES.toml, not \"seed\""
                    ));
                }
                return Ok(());
            }
            match self.id_to_wave.get(id) {
                Some(w) if w == "seed" => return Ok(()),
                Some(w) => {
                    return Err(format!(
                        "id {id:?} is assigned to wave {w:?} in WAVES.toml, not \"seed\""
                    ));
                }
                None => {
                    return Err(format!(
                        "id {id:?} in sources/seed/ is not listed under [waves].seed in WAVES.toml"
                    ));
                }
            }
        }
        // Non-seed waves (w5*): strict explicit assignment per M12.
        match self.id_to_wave.get(id) {
            Some(assigned) if assigned == wave => Ok(()),
            Some(assigned) => Err(format!(
                "id {id:?} is in wave directory {wave:?} but WAVES.toml assigns it to {assigned:?}"
            )),
            None => Err(format!(
                "id {id:?} in wave directory {wave:?} is not assigned in WAVES.toml"
            )),
        }
    }
}

/// Extract the wave directory name for a file directly under `<root>/<wave>/<file>.toml`.
///
/// Rejects files at the root level or inside nested subdirectories (`<wave>/<sub>/<file>.toml`).
pub fn extract_wave(root: &std::path::Path, file: &std::path::Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let wave = comps.next()?.as_os_str().to_str()?.to_owned();
    // Must be strictly `<wave>/<file>.toml` (reject nested subdirectories).
    comps.next()?;
    if comps.next().is_some() {
        return None;
    }
    Some(wave)
}
