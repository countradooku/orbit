//! `{{name}}` placeholders, for panel text, action arguments and slash
//! commands. Names are dotted paths into a JSON value (`item.title`,
//! `input.draft`, `count`); a missing name is empty. There is no logic and
//! no escaping language: what a template can do is look values up.

use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _, Value};

/// Replaces each `{{ path }}` with what `lookup` returns for it.
pub fn render(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                out.push_str(&lookup(name).unwrap_or_default());
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value at a dotted path (`a.b.0.c`) in `root`.
pub fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut value = root;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        value = match part.parse::<usize>() {
            Ok(i) if value.is_array() => value.as_array()?.get(i)?,
            _ => value.as_object()?.get(&part)?,
        };
    }
    Some(value)
}

/// A JSON value as text: strings bare, everything else as JSON.
pub fn text_of(value: &Value) -> String {
    match value.as_str() {
        Some(s) => s.to_owned(),
        None if value.is_null() => String::new(),
        None => value.to_string(),
    }
}

/// Renders against a JSON context.
pub fn render_json(text: &str, context: &Value) -> String {
    render(text, &|path| lookup(context, path).map(text_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_look_values_up() {
        let ctx: Value = sonic_rs::from_str(r#"{"name":"Ada","items":[{"t":"x"}],"n":3}"#).unwrap();
        assert_eq!(render_json("Hi {{ name }}: {{items.0.t}} {{n}} {{missing}}!", &ctx), "Hi Ada: x 3 !");
        assert_eq!(render_json("no {{ end", &ctx), "no {{ end");
        assert_eq!(render("{{args}}", &|p| (p == "args").then(|| "a b".into())), "a b");
    }
}
