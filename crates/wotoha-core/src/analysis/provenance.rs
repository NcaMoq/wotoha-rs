//! Provenance for analysis components.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::value::{Confidence, UnitInterval};

/// Broad source category for a component.  The enum intentionally describes
/// how a value was produced, rather than naming a particular inference stack.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMethod {
    Classical,
    Neural,
    Hybrid,
    Derived,
    Imported,
    #[default]
    Unknown,
}

impl AnalysisMethod {
    pub fn validate(&self) -> bool {
        true
    }
}

/// Stable identity of a model or algorithm implementation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl ModelIdentity {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Option<Self> {
        let name = name.into();
        let version = version.into();
        (!name.trim().is_empty() && !version.trim().is_empty()).then_some(Self {
            id: name,
            version,
            revision: None,
        })
    }

    pub fn named(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        (!name.trim().is_empty()).then_some(Self {
            id: name,
            version: "unknown".to_owned(),
            revision: None,
        })
    }

    pub fn validate(&self) -> bool {
        !self.id.trim().is_empty()
            && !self.version.trim().is_empty()
            && self
                .revision
                .as_deref()
                .is_none_or(|revision| !revision.trim().is_empty())
    }
}

/// Provenance attached to one independently computed component.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComponentProvenance {
    pub component: String,
    pub method: AnalysisMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl ComponentProvenance {
    pub fn new(component: impl Into<String>, method: AnalysisMethod) -> Option<Self> {
        let component = component.into();
        (!component.trim().is_empty()).then_some(Self {
            component,
            method,
            model: None,
            confidence: None,
            notes: None,
        })
    }

    pub fn validate(&self) -> bool {
        !self.component.trim().is_empty()
            && self.method.validate()
            && self.model.as_ref().is_none_or(ModelIdentity::validate)
            && self
                .confidence
                .as_ref()
                .is_none_or(|confidence| confidence.validate())
            && self
                .notes
                .as_deref()
                .is_none_or(|notes| !notes.trim().is_empty())
    }
}

impl Default for ComponentProvenance {
    fn default() -> Self {
        Self {
            component: "unknown".to_owned(),
            method: AnalysisMethod::Unknown,
            model: None,
            confidence: None,
            notes: None,
        }
    }
}

/// Provenance envelope for a complete V2 analysis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisProvenance {
    pub analyzer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub components: Vec<ComponentProvenance>,
    /// Named slots keep the production provenance shape explicit. The vector
    /// remains available for forward-compatible experimental components.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rhythm: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tonal: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocal: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cues: Option<ComponentProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overall_confidence: Option<UnitInterval>,
}

impl Default for AnalysisProvenance {
    fn default() -> Self {
        Self {
            analyzer: "unknown".to_owned(),
            schema_version: None,
            components: Vec::new(),
            rhythm: None,
            structure: None,
            tonal: None,
            vocal: None,
            energy: None,
            cue: None,
            cues: None,
            overall_confidence: None,
        }
    }
}

impl AnalysisProvenance {
    pub fn new(analyzer: impl Into<String>) -> Option<Self> {
        let analyzer = analyzer.into();
        (!analyzer.trim().is_empty()).then_some(Self {
            analyzer,
            schema_version: None,
            components: Vec::new(),
            rhythm: None,
            structure: None,
            tonal: None,
            vocal: None,
            energy: None,
            cue: None,
            cues: None,
            overall_confidence: None,
        })
    }

    /// Return provenance for a component by its canonical name.
    ///
    /// Production components are stored in their explicit named slots; the
    /// `components` vector is reserved for additional/experimental entries.
    /// `cue` and the older plural spelling `cues` refer to the same component.
    /// Invalid records with conflicting entries are still rejected by
    /// [`Self::validate`]; this accessor consistently prefers the named slot.
    pub fn component(&self, name: &str) -> Option<&ComponentProvenance> {
        let name = canonical_component_name(name);
        let named = match name {
            "rhythm" => self.rhythm.as_ref(),
            "structure" => self.structure.as_ref(),
            "tonal" => self.tonal.as_ref(),
            "vocal" => self.vocal.as_ref(),
            "energy" => self.energy.as_ref(),
            "cue" => self.cue.as_ref().or(self.cues.as_ref()),
            _ => None,
        };
        named.or_else(|| {
            self.components
                .iter()
                .find(|component| canonical_component_name(&component.component) == name)
        })
    }

    pub fn validate(&self) -> bool {
        !self.analyzer.trim().is_empty()
            && self
                .schema_version
                .as_deref()
                .is_none_or(|version| !version.trim().is_empty())
            && self.components.iter().all(ComponentProvenance::validate)
            && [
                self.rhythm.as_ref(),
                self.structure.as_ref(),
                self.tonal.as_ref(),
                self.vocal.as_ref(),
                self.energy.as_ref(),
                self.cue.as_ref(),
                self.cues.as_ref(),
            ]
            .into_iter()
            .flatten()
            .all(ComponentProvenance::validate)
            && named_component_is("rhythm", self.rhythm.as_ref())
            && named_component_is("structure", self.structure.as_ref())
            && named_component_is("tonal", self.tonal.as_ref())
            && named_component_is("vocal", self.vocal.as_ref())
            && named_component_is("energy", self.energy.as_ref())
            && self.cue.as_ref().is_none_or(|component| {
                component.component == "cue" || component.component == "cues"
            })
            && self.cues.as_ref().is_none_or(|component| {
                component.component == "cue" || component.component == "cues"
            })
            && component_names_are_unique(self)
            && self
                .overall_confidence
                .as_ref()
                .is_none_or(|confidence| confidence.validate())
    }
}

fn canonical_component_name(name: &str) -> &str {
    if name == "cues" { "cue" } else { name }
}

fn component_names_are_unique(provenance: &AnalysisProvenance) -> bool {
    let mut names = HashSet::with_capacity(provenance.components.len() + 7);
    provenance
        .components
        .iter()
        .map(|component| component.component.as_str())
        .chain(
            [
                provenance.rhythm.as_ref(),
                provenance.structure.as_ref(),
                provenance.tonal.as_ref(),
                provenance.vocal.as_ref(),
                provenance.energy.as_ref(),
                provenance.cue.as_ref(),
                provenance.cues.as_ref(),
            ]
            .into_iter()
            .flatten()
            .map(|component| component.component.as_str()),
        )
        .all(|name| names.insert(canonical_component_name(name)))
}

fn named_component_is(expected: &str, component: Option<&ComponentProvenance>) -> bool {
    component.is_none_or(|component| component.component == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_rejects_empty_identity_and_duplicate_components() {
        assert!(ModelIdentity::named("").is_none());
        let component = ComponentProvenance::new("rhythm", AnalysisMethod::Neural).unwrap();
        let mut provenance = AnalysisProvenance::new("test-analyzer").unwrap();
        provenance.components = vec![component.clone(), component];
        assert!(!provenance.validate());
    }

    #[test]
    fn provenance_rejects_duplicates_between_named_slots_and_extension_list() {
        let rhythm = ComponentProvenance::new("rhythm", AnalysisMethod::Neural).unwrap();
        let mut provenance = AnalysisProvenance::new("test-analyzer").unwrap();
        provenance.rhythm = Some(rhythm.clone());
        provenance.components.push(rhythm);

        assert!(!provenance.validate());
    }

    #[test]
    fn provenance_rejects_cue_cues_alias_collisions() {
        let mut provenance = AnalysisProvenance::new("test-analyzer").unwrap();
        provenance.cue = ComponentProvenance::new("cue", AnalysisMethod::Derived);
        provenance.cues = ComponentProvenance::new("cues", AnalysisMethod::Imported);
        assert!(!provenance.validate());

        provenance.cues = None;
        provenance
            .components
            .push(ComponentProvenance::new("cues", AnalysisMethod::Imported).unwrap());
        assert!(!provenance.validate());
    }

    #[test]
    fn canonical_component_accessor_prefers_named_slots_and_supports_extensions() {
        let mut provenance = AnalysisProvenance::new("test-analyzer").unwrap();
        provenance.rhythm = ComponentProvenance::new("rhythm", AnalysisMethod::Neural);
        provenance.cues = ComponentProvenance::new("cues", AnalysisMethod::Derived);
        provenance
            .components
            .push(ComponentProvenance::new("loudness", AnalysisMethod::Classical).unwrap());

        assert_eq!(provenance.component("rhythm"), provenance.rhythm.as_ref());
        assert_eq!(provenance.component("cue"), provenance.cues.as_ref());
        assert_eq!(provenance.component("cues"), provenance.cues.as_ref());
        assert_eq!(
            provenance.component("loudness"),
            provenance.components.first()
        );
    }

    #[test]
    fn mixed_neural_and_classical_components_are_valid_provenance() {
        let mut rhythm = ComponentProvenance::new("rhythm", AnalysisMethod::Neural).unwrap();
        rhythm.model = ModelIdentity::new("beat-this", "1.0");
        rhythm.confidence = Some(Confidence::new(0.91).unwrap());

        let mut loudness = ComponentProvenance::new("loudness", AnalysisMethod::Classical).unwrap();
        loudness.model = ModelIdentity::named("ebu-r128");
        loudness.confidence = Some(Confidence::new(0.99).unwrap());

        let mut provenance = AnalysisProvenance::new("automix-v2").unwrap();
        provenance.schema_version = Some("2".to_owned());
        provenance.components = vec![rhythm, loudness];
        provenance.overall_confidence = Some(UnitInterval::new(0.94).unwrap());

        assert!(provenance.validate());
        assert_eq!(provenance.components[0].method, AnalysisMethod::Neural);
        assert_eq!(provenance.components[1].method, AnalysisMethod::Classical);
    }
}
