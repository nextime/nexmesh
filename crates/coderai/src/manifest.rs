//! What a capability is, as data.
//!
//! The service behind this bridge exposes well over two hundred routes across
//! a dozen capability classes, and its own documentation is already out of
//! date against its own code -- its published capability document omits whole
//! subsystems that are registered and serving. Hard-coding any of that would
//! rot, and rot silently.
//!
//! So a capability is a row in a TOML file: where to call, how to find out
//! whether it is installed here, which models it offers, which request fields
//! to pass through, and where the answer is. Supporting a new one is an edit;
//! the service moving underneath us is a one-line change.
//!
//! ## Why paths are lists
//!
//! Every "where is the answer" is a *list* of candidate paths, tried in order.
//! The shape of these responses is not reliably documented, and guessing one
//! and failing loudly on a rename would make the bridge brittle in exactly
//! the way this design is meant to avoid. First path that yields something
//! wins; none of them yielding is reported as such.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub service: Service,
    #[serde(default, rename = "capability")]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub name: String,
    /// GET returning the service's own capability document, if it has one.
    #[serde(default)]
    pub capability_document: Option<String>,
    /// Paths within that document holding a list of endpoint strings.
    #[serde(default)]
    pub document_endpoints: Vec<String>,
    /// GET returning the models the service is serving.
    #[serde(default)]
    pub models_endpoint: Option<String>,
    #[serde(default)]
    pub models_at: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    /// The nexmesh capability label teammates address, e.g. "ocr".
    pub label: String,
    /// One line for `list_peers`, so a teammate can tell what it does.
    pub summary: String,
    /// The path called to do the work.
    pub endpoint: String,
    #[serde(default = "post")]
    pub method: String,
    /// A GET listing this capability's own engines or models. Its answering
    /// at all is the strongest evidence the capability is installed.
    #[serde(default)]
    pub probe: Option<String>,
    /// Where, in the probe's answer, the model ids are.
    #[serde(default)]
    pub models_at: Vec<String>,
    /// Which request field names the model choice, when it takes one.
    #[serde(default)]
    pub model_field: Option<String>,
    /// Request fields passed through from the asker, and which are required.
    #[serde(default)]
    pub accepts: Vec<String>,
    #[serde(default)]
    pub required: Vec<String>,
    /// Where the useful text is in the answer.
    #[serde(default)]
    pub text_at: Vec<String>,
    /// Where a produced file's url or name is.
    #[serde(default)]
    pub file_at: Vec<String>,
    /// Treat this capability as absent unless its probe answers, even if the
    /// service's own document lists it. For anything expensive enough that a
    /// wrong "yes" would waste real time.
    #[serde(default)]
    pub probe_required: bool,
}

fn post() -> String {
    "POST".into()
}

impl Manifest {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let m: Manifest = toml::from_str(text)?;
        if m.capabilities.is_empty() {
            anyhow::bail!("the manifest declares no capabilities");
        }
        for c in &m.capabilities {
            if c.label.trim().is_empty() {
                anyhow::bail!("a capability has no label");
            }
            if c.label != c.label.to_lowercase() {
                // Labels are addressed by agents, and matching is exact.
                anyhow::bail!("capability label '{}' must be lower case", c.label);
            }
            for r in &c.required {
                if !c.accepts.contains(r) {
                    anyhow::bail!(
                        "capability '{}' requires '{r}' but does not accept it",
                        c.label
                    );
                }
            }
        }
        Ok(m)
    }

    pub fn find(&self, label: &str) -> Option<&Capability> {
        let l = label.trim().to_lowercase();
        self.capabilities.iter().find(|c| c.label == l)
    }
}

/// Read a dotted path out of JSON, with `[*]` to walk an array.
///
/// `data[*].id` over `{"data":[{"id":"a"},{"id":"b"}]}` gives `["a","b"]`.
/// An empty path means the value itself, so `[*].id` works on a bare array.
pub fn extract(value: &serde_json::Value, path: &str) -> Vec<String> {
    let mut current: Vec<&serde_json::Value> = vec![value];
    for part in path.split('.') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, splat) = match part.strip_suffix("[*]") {
            Some(k) => (k, true),
            None => (part, false),
        };
        let mut next: Vec<&serde_json::Value> = Vec::new();
        for v in current {
            let picked = if key.is_empty() { Some(v) } else { v.get(key) };
            match picked {
                Some(inner) if splat => {
                    if let Some(arr) = inner.as_array() {
                        next.extend(arr.iter());
                    }
                }
                Some(inner) => next.push(inner),
                None => {}
            }
        }
        current = next;
    }
    current
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// First path that yields anything.
pub fn extract_first(value: &serde_json::Value, paths: &[String]) -> Vec<String> {
    for p in paths {
        let got = extract(value, p);
        if !got.is_empty() {
            return got;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"
[service]
name = "coderai"
capability_document = "/coderai/capabilities"
document_endpoints = ["studio.endpoints[*]"]

[[capability]]
label = "ocr"
summary = "Read text out of an image or a PDF"
endpoint = "/v1/ocr"
probe = "/v1/ocr/engines"
models_at = ["data[*].id", "[*].id"]
model_field = "engine"
accepts = ["image", "lang", "structured"]
required = ["image"]
text_at = ["text", "data.text"]
"#;

    #[test]
    fn a_manifest_parses_and_can_be_looked_up() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.service.name, "coderai");
        let c = m.find("OCR").expect("case-insensitive lookup");
        assert_eq!(c.endpoint, "/v1/ocr");
        assert_eq!(c.method, "POST", "the default");
        assert_eq!(c.model_field.as_deref(), Some("engine"));
    }

    #[test]
    fn an_upper_case_label_is_refused() {
        // Agents address labels and matching is exact, so a capital letter
        // here would be a capability nobody can reach.
        let bad = SAMPLE.replace(r#"label = "ocr""#, r#"label = "OCR""#);
        let err = Manifest::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("lower case"), "{err}");
    }

    #[test]
    fn requiring_a_field_it_does_not_accept_is_refused() {
        // Otherwise the bridge would demand something it then discards.
        let bad = SAMPLE.replace(r#"required = ["image"]"#, r#"required = ["nonsense"]"#);
        let err = Manifest::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("requires 'nonsense'"), "{err}");
    }

    #[test]
    fn a_manifest_with_no_capabilities_is_refused() {
        let err = Manifest::parse("[service]\nname = \"x\"\n").unwrap_err().to_string();
        assert!(err.contains("no capabilities"), "{err}");
    }

    #[test]
    fn a_path_reads_a_nested_value() {
        let v = json!({"data": {"text": "hello"}});
        assert_eq!(extract(&v, "data.text"), vec!["hello"]);
    }

    #[test]
    fn a_splat_walks_an_array() {
        let v = json!({"data": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(extract(&v, "data[*].id"), vec!["a", "b"]);
    }

    #[test]
    fn a_splat_works_on_a_bare_array() {
        let v = json!([{"id": "x"}, {"id": "y"}]);
        assert_eq!(extract(&v, "[*].id"), vec!["x", "y"]);
    }

    #[test]
    fn numbers_and_booleans_come_back_as_text() {
        let v = json!({"n": 3, "ok": true});
        assert_eq!(extract(&v, "n"), vec!["3"]);
        assert_eq!(extract(&v, "ok"), vec!["true"]);
    }

    #[test]
    fn a_missing_path_is_empty_rather_than_an_error() {
        let v = json!({"a": 1});
        assert!(extract(&v, "b.c.d").is_empty());
        assert!(extract(&v, "a[*].x").is_empty());
    }

    #[test]
    fn the_first_path_that_yields_anything_wins() {
        // The whole point: these response shapes are not reliably documented,
        // so a rename must cost a fallback rather than the capability.
        let v = json!({"engines": [{"id": "surya"}]});
        let paths = vec!["data[*].id".to_string(), "engines[*].id".to_string()];
        assert_eq!(extract_first(&v, &paths), vec!["surya"]);
    }

    #[test]
    fn no_path_yielding_anything_is_reported_as_empty_not_guessed() {
        let v = json!({"something": "else"});
        let paths = vec!["data[*].id".to_string(), "engines[*].id".to_string()];
        assert!(extract_first(&v, &paths).is_empty());
    }

    #[test]
    fn empty_strings_are_not_treated_as_values() {
        let v = json!({"data": [{"id": ""}, {"id": "real"}]});
        assert_eq!(extract(&v, "data[*].id"), vec!["real"]);
    }

    #[test]
    fn the_real_capability_document_shape_yields_its_endpoint_list() {
        // The shape actually served: studio.endpoints as a flat array.
        let v = json!({
            "server": "codai",
            "studio": { "supported": true, "endpoints": ["v1/audio/speech", "v1/ocr"] }
        });
        let got = extract(&v, "studio.endpoints[*]");
        assert_eq!(got, vec!["v1/audio/speech", "v1/ocr"]);
    }
}
