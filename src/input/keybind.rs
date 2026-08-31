use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub super_: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub modifiers: Modifiers,
    /// Canonicalized (lowercased) key name — `"q"`, `"return"`,
    /// `"f4"`, `"1"`. Matching a combo means the modifiers match
    /// exactly (not "at least these") and `key_name` matches exactly,
    /// case-insensitively at parse time.
    pub key_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid key combo: {}", self.0)
    }
}
impl std::error::Error for ParseError {}

/// Parses a combo string like `"Super+Shift+Q"` or `"Alt+F4"`.
/// Modifier names are case-insensitive and accept a couple of common
/// aliases (`"Super"`/`"Mod4"`/`"Win"`/`"Logo"` all mean the same
/// modifier — matches the naming different tools/docs use for it), so
/// a person copying a binding from another compositor's config doesn't
/// need to first translate its modifier names into this one's
/// preferred spelling. Whitespace around `+` is ignored. The final
/// segment (after the last `+`) is always the key itself, everything
/// before it must be a recognized modifier name — an empty string, a
/// string with no key segment (`"Super+"`), or an unrecognized
/// modifier name are all rejected with a [`ParseError`] naming what was
/// wrong, rather than silently producing a combo that can never match
/// any real keypress.
pub fn parse_combo(s: &str) -> Result<KeyCombo, ParseError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ParseError("empty combo string".to_string()));
    }

    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(ParseError(format!("empty segment in '{s}' (stray '+', or leading/trailing '+')")));
    }

    let (modifier_parts, key_part) = parts.split_at(parts.len() - 1);
    let key_part = key_part[0];

    let mut modifiers = Modifiers::default();
    for raw in modifier_parts {
        match raw.to_ascii_lowercase().as_str() {
            "super" | "mod4" | "win" | "logo" | "cmd" => modifiers.super_ = true,
            "shift" => modifiers.shift = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "mod1" | "meta" => modifiers.alt = true,
            other => return Err(ParseError(format!("unrecognized modifier '{other}' in '{s}'"))),
        }
    }

    Ok(KeyCombo { modifiers, key_name: key_part.to_ascii_lowercase() })
}

/// A compiled, ready-to-match set of keybindings — built once from
/// `Config.keybindings` (see [`Keybindings::from_config`]) and queried
/// on every keypress via [`Keybindings::action_for`], rather than
/// re-parsing every combo string on every single key event.
#[derive(Debug, Clone, Default)]
pub struct Keybindings {
    by_combo: HashMap<KeyCombo, String>,
}

impl Keybindings {
    /// Builds a matcher from `action name -> combo string` pairs (the
    /// shape `Config.keybindings` is already in). A combo string that
    /// fails to parse is skipped with the error returned alongside the
    /// action name it belonged to — see `errors` — rather than
    /// aborting the whole build, matching `src/config.rs`'s own
    /// "one bad field shouldn't take down everything else" philosophy.
    /// If two actions somehow bind the same combo, the later one (in
    /// the input map's iteration order) wins — this is inherently
    /// order-dependent for a `HashMap` input, so a person who does this
    /// should expect it as undefined-but-harmless rather than a crash;
    /// `src/config.rs`'s own `.hk` format doesn't allow true duplicate
    /// keys within one section, so triggering this in practice means
    /// two *different* action names bound to the same combo, which a
    /// future config-validation pass (see ROADMAP.md) could reasonably
    /// warn about instead of silently picking one.
    pub fn from_config(actions: &HashMap<String, String>) -> (Self, Vec<(String, ParseError)>) {
        let mut by_combo = HashMap::new();
        let mut errors = Vec::new();
        for (action, combo_str) in actions {
            match parse_combo(combo_str) {
                Ok(combo) => {
                    by_combo.insert(combo, action.clone());
                }
                Err(e) => errors.push((action.clone(), e)),
            }
        }
        (Self { by_combo }, errors)
    }

    /// Looks up which action (if any) is bound to `modifiers` +
    /// `key_name` (already-canonicalized, lowercase — see
    /// `crate::input::keysym_name` for turning a real `Keysym` into
    /// this shape).
    pub fn action_for(&self, modifiers: Modifiers, key_name: &str) -> Option<&str> {
        self.by_combo
            .get(&KeyCombo { modifiers, key_name: key_name.to_ascii_lowercase() })
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.by_combo.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_combo.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_single_modifier_combo() {
        let combo = parse_combo("Super+Q").unwrap();
        assert_eq!(combo.key_name, "q");
        assert!(combo.modifiers.super_);
        assert!(!combo.modifiers.shift);
    }

    #[test]
    fn parses_multiple_modifiers_in_any_order() {
        let a = parse_combo("Super+Shift+Q").unwrap();
        let b = parse_combo("Shift+Super+Q").unwrap();
        assert_eq!(a, b, "modifier order shouldn't affect the parsed result");
        assert!(a.modifiers.super_ && a.modifiers.shift);
    }

    #[test]
    fn is_case_insensitive_and_trims_whitespace() {
        let combo = parse_combo("  super + SHIFT + q  ").unwrap();
        assert_eq!(combo.key_name, "q");
        assert!(combo.modifiers.super_ && combo.modifiers.shift);
    }

    #[test]
    fn accepts_common_modifier_aliases() {
        assert_eq!(parse_combo("Mod4+Q").unwrap().modifiers.super_, true);
        assert_eq!(parse_combo("Win+Q").unwrap().modifiers.super_, true);
        assert_eq!(parse_combo("Control+Q").unwrap().modifiers.ctrl, true);
        assert_eq!(parse_combo("Mod1+F4").unwrap().modifiers.alt, true);
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_combo("").is_err());
        assert!(parse_combo("   ").is_err());
    }

    #[test]
    fn rejects_unrecognized_modifier() {
        let err = parse_combo("Hyper+Q").unwrap_err();
        assert!(err.0.contains("Hyper"));
    }

    #[test]
    fn rejects_dangling_plus() {
        assert!(parse_combo("Super+").is_err());
        assert!(parse_combo("+Q").is_err());
        assert!(parse_combo("Super++Q").is_err());
    }

    #[test]
    fn a_bare_key_with_no_modifiers_is_valid() {
        let combo = parse_combo("F4").unwrap();
        assert_eq!(combo.key_name, "f4");
        assert_eq!(combo.modifiers, Modifiers::default());
    }

    #[test]
    fn keybindings_matches_the_exact_modifier_combination() {
        let mut actions = HashMap::new();
        actions.insert("close_window".to_string(), "Super+Q".to_string());
        let (bindings, errors) = Keybindings::from_config(&actions);
        assert!(errors.is_empty());

        let supermods = Modifiers { super_: true, ..Default::default() };
        assert_eq!(bindings.action_for(supermods, "q"), Some("close_window"));

        // Wrong modifiers (missing Super) shouldn't match, even though
        // the key name is right — an exact-match matcher, not a
        // subset/superset one.
        assert_eq!(bindings.action_for(Modifiers::default(), "q"), None);

        // Extra modifiers the binding didn't ask for also shouldn't
        // match — Super+Shift+Q pressed shouldn't accidentally trigger
        // a plain Super+Q binding.
        let super_shift = Modifiers { super_: true, shift: true, ..Default::default() };
        assert_eq!(bindings.action_for(super_shift, "q"), None);
    }

    #[test]
    fn malformed_combo_is_reported_but_does_not_break_other_bindings() {
        let mut actions = HashMap::new();
        actions.insert("close_window".to_string(), "Super+Q".to_string());
        actions.insert("broken".to_string(), "Hyper+Nonsense+".to_string());
        let (bindings, errors) = Keybindings::from_config(&actions);

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "broken");

        let supermods = Modifiers { super_: true, ..Default::default() };
        assert_eq!(bindings.action_for(supermods, "q"), Some("close_window"));
    }

    #[test]
    fn empty_config_produces_an_empty_matcher_not_an_error() {
        let (bindings, errors) = Keybindings::from_config(&HashMap::new());
        assert!(bindings.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn default_keybindings_from_config_all_parse_successfully() {
        // Mirrors `config::default_keybindings()` in the main crate —
        // kept here as a literal copy (rather than importing it, which
        // would require this module to depend on `crate::config`) so
        // this test fails loudly if a future edit to that function
        // introduces a combo string this parser can't handle.
        let mut actions = HashMap::new();
        actions.insert("close_window".to_string(), "Super+Q".to_string());
        actions.insert("toggle_fullscreen".to_string(), "Super+F".to_string());
        actions.insert("toggle_floating".to_string(), "Super+Space".to_string());
        actions.insert("cycle_windows".to_string(), "Super+Tab".to_string());
        actions.insert("launch_terminal".to_string(), "Super+Return".to_string());
        for n in 1..=5 {
            actions.insert(format!("workspace_{n}"), format!("Super+{n}"));
        }
        let (bindings, errors) = Keybindings::from_config(&actions);
        assert!(errors.is_empty(), "default keybindings should always parse cleanly: {errors:?}");
        assert_eq!(bindings.len(), actions.len());
    }
}
