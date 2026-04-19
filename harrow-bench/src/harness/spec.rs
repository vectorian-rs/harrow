use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Single,
    Compare,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Compare => "compare",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentMode {
    Local,
    Remote,
}

impl DeploymentMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadGeneratorKind {
    Spinr,
    Wrk3,
}

impl LoadGeneratorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spinr => "spinr",
            Self::Wrk3 => "wrk3",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImplementationSpec {
    pub id: String,
    pub image: String,
    pub command: String,
    #[serde(default)]
    pub build_task: Option<String>,
    #[serde(default)]
    pub framework: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub health_path: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

impl ImplementationSpec {
    pub fn health_path(&self) -> &str {
        self.health_path.as_deref().unwrap_or("/health")
    }

    pub fn framework_label(&self) -> &str {
        self.framework.as_deref().unwrap_or(self.id.as_str())
    }

    pub fn backend_label(&self) -> &str {
        self.backend.as_deref().unwrap_or("unknown")
    }

    pub fn profile_label(&self) -> &str {
        self.profile.as_deref().unwrap_or("unknown")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ImplementationRegistry {
    #[serde(rename = "implementation")]
    pub implementations: Vec<ImplementationSpec>,
}

impl ImplementationRegistry {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read implementation registry {}: {e}",
                path.display()
            )
        })?;
        let expanded = expand_registry_vars(&text);
        toml::from_str(&expanded).map_err(|e| {
            format!(
                "failed to parse implementation registry {}: {e}",
                path.display()
            )
        })
    }

    pub fn get(&self, id: &str) -> Option<&ImplementationSpec> {
        self.implementations.iter().find(|spec| spec.id == id)
    }
}

fn expand_registry_vars(text: &str) -> String {
    text.replace(
        "${HARROW_VERSION}",
        &std::env::var("HARROW_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SuiteSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "case")]
    pub cases: Vec<CaseSpec>,
}

impl SuiteSpec {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read(path).map_err(|e| format!("failed to read suite {}: {e}", path.display()))?;
        toml::from_str(&String::from_utf8_lossy(&text))
            .map_err(|e| format!("failed to parse suite {}: {e}", path.display()))
    }

    pub fn selected_cases<'a>(&'a self, filters: &[String]) -> Result<Vec<&'a CaseSpec>, String> {
        if filters.is_empty() {
            return Ok(self.cases.iter().collect());
        }

        let mut out = Vec::with_capacity(filters.len());
        for filter in filters {
            let case = self
                .cases
                .iter()
                .find(|case| case.id == *filter)
                .ok_or_else(|| {
                    format!("suite '{}' does not contain case '{}'", self.name, filter)
                })?;
            out.push(case);
        }
        Ok(out)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaseSpec {
    pub id: String,
    pub generator: LoadGeneratorKind,
    pub template: PathBuf,
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub rate: Option<u32>,
    #[serde(default)]
    pub duration_secs: Option<u32>,
    #[serde(default)]
    pub warmup_secs: Option<u32>,
    #[serde(default)]
    pub server_flags: Vec<String>,
    #[serde(default)]
    pub context: BTreeMap<String, toml::Value>,
}

impl CaseSpec {
    pub fn resolved_template_path(&self, suite_path: &Path) -> PathBuf {
        if self.template.is_absolute() {
            return self.template.clone();
        }

        suite_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&self.template)
    }
}
