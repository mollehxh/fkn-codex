use serde_json::Value;
use serde_json::json;

const MAX_SKILLS: usize = 128;
const MAX_SKILL_ERRORS: usize = 16;

pub(crate) fn compact_skills_catalog(catalog: &Value) -> Value {
    let mut skills = catalog
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|entry| {
            entry
                .get("skills")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|skill| skill.get("enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(compact_skill_metadata)
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    skills.dedup_by(|left, right| left.get("name") == right.get("name"));

    let total = skills.len();
    skills.truncate(MAX_SKILLS);
    let errors = catalog
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|entry| {
            entry
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|error| error.get("message").and_then(Value::as_str))
        .take(MAX_SKILL_ERRORS)
        .collect::<Vec<_>>();

    json!({
        "skills": skills,
        "returned": skills.len(),
        "total": total,
        "truncated": total > skills.len(),
        "errors": errors,
    })
}

pub(crate) fn compact_skill_metadata(skill: &Value) -> Option<Value> {
    let name = skill.get("name")?.as_str()?;
    let description = skill.get("description")?.as_str()?;
    let mut compact = serde_json::Map::from_iter([
        ("name".to_string(), Value::String(name.to_string())),
        (
            "description".to_string(),
            Value::String(description.to_string()),
        ),
    ]);
    if let Some(scope) = skill.get("scope").and_then(Value::as_str) {
        compact.insert("scope".to_string(), Value::String(scope.to_string()));
    }
    if let Some(plugin_id) = skill.get("pluginId").and_then(Value::as_str) {
        compact.insert(
            "plugin_id".to_string(),
            Value::String(plugin_id.to_string()),
        );
    }
    Some(Value::Object(compact))
}
