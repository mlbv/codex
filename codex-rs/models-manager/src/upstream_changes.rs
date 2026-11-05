//! Human-readable changes in upstream model metadata relative to the bundled catalog.

use codex_protocol::openai_models::ModelInfo;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use similar::TextDiff;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;

/// Publishes one complete report per distinct decoded upstream catalog.
///
/// Model-list and JSON object-key order do not affect the revision identity. The report filename
/// is the deduplication record shared by concurrent processes and later sessions.
pub async fn record_upstream_changes(
    directory: &Path,
    models: &[ModelInfo],
) -> io::Result<Option<PathBuf>> {
    let directory = directory.to_path_buf();
    let models = models.to_vec();
    tokio::task::spawn_blocking(move || record(&directory, &models))
        .await
        .map_err(io::Error::other)?
}

fn record(directory: &Path, models: &[ModelInfo]) -> io::Result<Option<PathBuf>> {
    let bundled = crate::bundled_models_response().map_err(io::Error::other)?;
    let bundled = canonical_catalog(&bundled.models)?;
    let upstream = canonical_catalog(models)?;
    if bundled == upstream {
        return Ok(None);
    }

    let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&upstream)?));
    let path = directory.join(format!("model-catalog-{revision}.md"));
    if path.try_exists()? {
        return Ok(None);
    }

    let bundled_revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&bundled)?));
    let report = render_report(&bundled, &upstream, &revision, &bundled_revision);
    std::fs::create_dir_all(directory)?;
    let mut temporary = NamedTempFile::new_in(directory)?;
    temporary.write_all(report.as_bytes())?;
    temporary.flush()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error.error),
    }
}

fn canonical_catalog(models: &[ModelInfo]) -> io::Result<Value> {
    let mut models = models
        .iter()
        .map(|model| serde_json::to_value(model).map(canonical_value))
        .collect::<Result<Vec<_>, _>>()?;
    models.sort_by_cached_key(|model| {
        (
            model["slug"].as_str().unwrap().to_owned(),
            model.to_string(),
        )
    });
    Ok(Value::Array(models))
}

fn canonical_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical_value(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(array) => Value::Array(array.into_iter().map(canonical_value).collect()),
        value => value,
    }
}

fn catalog_by_slug(catalog: &Value) -> BTreeMap<&str, Vec<&Value>> {
    let mut result = BTreeMap::<_, Vec<_>>::new();
    for model in catalog.as_array().unwrap() {
        result
            .entry(model["slug"].as_str().unwrap())
            .or_default()
            .push(model);
    }
    result
}

fn render_report(
    bundled: &Value,
    upstream: &Value,
    revision: &str,
    bundled_revision: &str,
) -> String {
    let bundled_models = catalog_by_slug(bundled);
    let upstream_models = catalog_by_slug(upstream);
    let slugs = bundled_models
        .keys()
        .chain(upstream_models.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut added = 0;
    let mut removed = 0;
    let mut changed = 0;
    let mut details = String::new();

    for slug in slugs {
        let old = bundled_models.get(slug);
        let new = upstream_models.get(slug);
        if old == new {
            continue;
        }
        let change = match (old, new) {
            (None, Some(_)) => {
                added += 1;
                "Added"
            }
            (Some(_), None) => {
                removed += 1;
                "Removed"
            }
            _ => {
                changed += 1;
                "Changed"
            }
        };
        writeln!(details, "## {change} model {}\n", inline_code(slug)).unwrap();
        match (old.map(Vec::as_slice), new.map(Vec::as_slice)) {
            (None | Some([_]), None | Some([_])) => render_fields(
                &mut details,
                "",
                old.map(|models| models[0]),
                new.map(|models| models[0]),
            ),
            _ => {
                let old = old.map(|models| {
                    Value::Array(models.iter().map(|model| (*model).clone()).collect())
                });
                let new = new.map(|models| {
                    Value::Array(models.iter().map(|model| (*model).clone()).collect())
                });
                render_fields(
                    &mut details,
                    "model declarations",
                    old.as_ref(),
                    new.as_ref(),
                );
            }
        }
    }

    let mut report = format!(
        "# Upstream model catalog changes\n\nRecorded: {}\n\nUpstream revision: `{revision}`\n\nBundled baseline: codex-models-manager {} (`models.json`), revision `{bundled_revision}`.\n\n{} bundled models; {} upstream models.\n\n{added} added, {removed} removed, {changed} changed.\n\nThe changes below compare bundled values (`-`) with upstream values (`+`). Field paths name decoded model metadata; `(missing)` means the field is absent.\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        env!("CARGO_PKG_VERSION"),
        bundled.as_array().unwrap().len(),
        upstream.as_array().unwrap().len(),
    );
    report.push_str(&details);
    report
}

fn render_fields(report: &mut String, path: &str, old: Option<&Value>, new: Option<&Value>) {
    if old == new {
        return;
    }
    let old_object = old.and_then(Value::as_object);
    let new_object = new.and_then(Value::as_object);
    if (old.is_none() || old_object.is_some())
        && (new.is_none() || new_object.is_some())
        && (old_object.is_some() || new_object.is_some())
    {
        let keys = old_object
            .into_iter()
            .flat_map(|object| object.keys())
            .chain(new_object.into_iter().flat_map(|object| object.keys()))
            .collect::<BTreeSet<_>>();
        if !keys.is_empty() {
            for key in keys {
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                render_fields(
                    report,
                    &field_path,
                    old_object.and_then(|object| object.get(key)),
                    new_object.and_then(|object| object.get(key)),
                );
            }
            return;
        }
    }

    writeln!(report, "### {}\n", inline_code(path)).unwrap();
    let multiline_string = old
        .and_then(Value::as_str)
        .into_iter()
        .chain(new.and_then(Value::as_str))
        .any(|text| text.contains('\n'));
    let strings_or_null = [old, new]
        .into_iter()
        .all(|value| value.is_none_or(|value| value.is_string() || value.is_null()));
    let old_text = field_text(old, multiline_string && strings_or_null);
    let new_text = field_text(new, multiline_string && strings_or_null);
    let diff = TextDiff::from_lines(&old_text, &new_text)
        .unified_diff()
        .context_radius(3)
        .header("bundled", "upstream")
        .to_string();
    let fence = "`".repeat(longest_backtick_run(&diff).max(2) + 1);
    writeln!(report, "{fence}diff\n{diff}{fence}\n").unwrap();
}

fn field_text(value: Option<&Value>, literal_string: bool) -> String {
    let mut text = match value {
        None => "(missing)".to_owned(),
        Some(Value::String(text)) if literal_string => return text.clone(),
        Some(value) => serde_json::to_string_pretty(value).unwrap(),
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn inline_code(text: &str) -> String {
    let delimiter = "`".repeat(longest_backtick_run(text) + 1);
    format!("{delimiter} {text} {delimiter}")
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap()
}
