//! JSON, JSON with comments and YAML documents read into one value type.

use anyhow::{Result, anyhow};
use serde_json::{Map, Number, Value};
use yaml_rust2::{Yaml, YamlLoader};

pub(crate) fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| anyhow!("invalid JSON: {error}"))
}

pub(crate) fn parse_jsonc(text: &str) -> Result<Value> {
    let mut text = text.to_owned();

    json_strip_comments::strip(&mut text).map_err(|error| anyhow!("invalid JSON: {error}"))?;

    parse_json(&text)
}

pub(crate) fn parse_yaml(text: &str) -> Result<Value> {
    let documents = parse_yaml_documents(text)?;

    Ok(documents.into_iter().next().unwrap_or(Value::Null))
}

pub(crate) fn parse_yaml_documents(text: &str) -> Result<Vec<Value>> {
    let documents =
        YamlLoader::load_from_str(text).map_err(|error| anyhow!("invalid YAML: {error}"))?;

    Ok(documents.into_iter().map(to_value).collect())
}

fn to_value(yaml: Yaml) -> Value {
    match yaml {
        Yaml::Real(real) => real
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or(Value::String(real), Value::Number),
        Yaml::Integer(integer) => Value::Number(integer.into()),
        Yaml::String(string) => Value::String(string),
        Yaml::Boolean(boolean) => Value::Bool(boolean),
        Yaml::Array(items) => Value::Array(items.into_iter().map(to_value).collect()),
        Yaml::Hash(entries) => {
            let map: Map<String, Value> = entries
                .into_iter()
                .map(|(key, value)| (key_text(key), to_value(value)))
                .collect();

            Value::Object(map)
        }
        Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => Value::Null,
    }
}

fn key_text(key: Yaml) -> String {
    match key {
        Yaml::Real(text) | Yaml::String(text) => text,
        Yaml::Integer(integer) => integer.to_string(),
        Yaml::Boolean(boolean) => boolean.to_string(),
        Yaml::Array(_) | Yaml::Hash(_) | Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn yaml_maps_lists_and_scalars_become_json_values() {
        let text = "
lockfileVersion: '9.0'
catalog:
  vue: ^3.5.0
  2: two
packages:
  - packages/*
retries: 3
ratio: 0.5
strict: true
empty: ~
";

        assert_eq!(
            parse_yaml(text).unwrap(),
            json!({
                "lockfileVersion": "9.0",
                "catalog": { "vue": "^3.5.0", "2": "two" },
                "packages": ["packages/*"],
                "retries": 3,
                "ratio": 0.5,
                "strict": true,
                "empty": null,
            })
        );
    }

    #[test]
    fn json_with_comments_and_trailing_commas_is_read_like_tsconfig() {
        let text = r#"{
            // compiler options
            "compilerOptions": { "strict": true, /* inline */ },
        }"#;

        assert_eq!(
            parse_jsonc(text).unwrap(),
            json!({ "compilerOptions": { "strict": true } })
        );
    }

    #[test]
    fn invalid_documents_are_errors() {
        assert!(parse_json("{").is_err());
        assert!(parse_jsonc("{ \"a\": }").is_err());
        assert!(parse_yaml("key: [unclosed").is_err());
    }
}
