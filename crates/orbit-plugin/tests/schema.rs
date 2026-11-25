//! The published JSON Schema (`docs/orbit-plugin.schema.json`) describes
//! exactly the keys the manifest parser accepts, and the bundled examples
//! validate.

use std::collections::BTreeSet;
use std::path::Path;

use orbit_plugin::agent::{AgentManifest, ModeEntry, ModelEntry};
use orbit_plugin::manifest::*;
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

/// The field names serde's derive gives `T`.
fn fields_of<T: DeserializeOwned>() -> BTreeSet<String> {
    struct Probe<'a>(&'a mut Option<&'static [&'static str]>);
    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }
        fn deserialize_struct<V: Visitor<'de>>(self, _: &'static str, fields: &'static [&'static str], _: V) -> Result<V::Value, Self::Error> {
            *self.0 = Some(fields);
            Err(de::Error::custom("probed"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit
            unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
        }
    }
    let mut fields = None;
    let _ = T::deserialize(Probe(&mut fields));
    fields.expect("a struct").iter().map(|f| (*f).to_owned()).collect()
}

fn schema() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/orbit-plugin.schema.json");
    sonic_rs::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn properties(node: &Value) -> BTreeSet<String> {
    node["properties"].as_object().unwrap().iter().map(|(k, _)| k.to_owned()).collect()
}

fn enum_of(node: &Value) -> BTreeSet<String> {
    node["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect()
}

#[test]
fn the_schema_names_exactly_the_manifest_keys() {
    let schema = schema();
    let defs = &schema["$defs"];
    assert_eq!(properties(&schema), fields_of::<Manifest>());
    assert_eq!(properties(&schema["properties"]["engines"]), fields_of::<Engines>());
    let pairs: Vec<(&str, BTreeSet<String>)> = vec![
        ("theme", fields_of::<ThemeEntry>()),
        ("snippet", fields_of::<SnippetEntry>()),
        ("slash_command", fields_of::<SlashCommandEntry>()),
        ("recipe", fields_of::<RecipeEntry>()),
        ("keybinding", fields_of::<KeybindingEntry>()),
        ("template", fields_of::<TemplateEntry>()),
        ("agent", fields_of::<AgentManifest>()),
        ("panel", fields_of::<PanelEntry>()),
        ("command", fields_of::<CommandEntry>()),
        ("setting", fields_of::<SettingEntry>()),
        ("wasm", fields_of::<WasmEntry>()),
    ];
    for (def, fields) in pairs {
        assert_eq!(properties(&defs[def]), fields, "$defs.{def}");
        assert_eq!(defs[def]["additionalProperties"].as_bool(), Some(false), "$defs.{def} refuses unknown keys");
    }
    assert_eq!(properties(&defs["agent"]["properties"]["models"]["items"]), fields_of::<ModelEntry>());
    assert_eq!(properties(&defs["agent"]["properties"]["modes"]["items"]), fields_of::<ModeEntry>());
    assert_eq!(schema["additionalProperties"].as_bool(), Some(false));
}

#[test]
fn the_schema_lists_every_capability_and_hook() {
    let schema = schema();
    let caps: BTreeSet<String> = orbit_proto::Capability::ALL.iter().map(|c| c.key().to_owned()).collect();
    assert_eq!(enum_of(&schema["properties"]["capabilities"]["items"]), caps);
    let hooks: BTreeSet<String> = HOOKS.iter().map(|(h, _)| (*h).to_owned()).collect();
    assert_eq!(enum_of(&schema["$defs"]["wasm"]["properties"]["hooks"]["items"]), hooks);
}

#[test]
fn every_bundled_example_validates() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
    let mut seen = 0;
    for entry in std::fs::read_dir(examples).unwrap() {
        let dir = entry.unwrap().path();
        if dir.join(MANIFEST).is_file() {
            let loaded = orbit_plugin::package::load(&dir).unwrap_or_else(|p| panic!("{}: {}", dir.display(), p.join("; ")));
            assert!(loaded.warnings.is_empty(), "{}: {:?}", dir.display(), loaded.warnings);
            seen += 1;
        }
    }
    assert_eq!(seen, 3);
}
