//! User preferences: how the browser looks and what it is allowed to do.
//!
//! Settings are edited from `zero://settings`, whose controls are ordinary links
//! carrying the new value (`zero://settings?rail=icons`). Changing a preference
//! therefore goes through the same navigation path as clicking any link — no
//! widget toolkit in the shell, and every setting is addressable.
//!
//! Every field is `Copy` so the whole struct can be read cheaply from anywhere,
//! including the subresource loader deciding whether to block a request.

use std::cell::RefCell;

/// Where the tab strip lives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabLayout {
    /// A rail down the left edge — Zero's default, per docs/02-UI-UX-SPEC.md §4.
    Vertical,
    /// A strip across the top, as most browsers do it.
    Horizontal,
}

/// How much of the vertical rail is showing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rail {
    Expanded,
    /// A narrow strip of initials — you keep your place without spending width.
    Icons,
    Hidden,
}

impl Rail {
    /// The next state for the collapse control, which cycles rather than toggles.
    pub fn next(self) -> Rail {
        match self {
            Rail::Expanded => Rail::Icons,
            Rail::Icons => Rail::Hidden,
            Rail::Hidden => Rail::Expanded,
        }
    }
}

/// Search engines offered for the address bar and the new-tab field.
///
/// Each is a key, a label, and the prefix a percent-encoded query is appended to.
/// All four work without JavaScript, which Zero's engine does not run for them.
pub const ENGINES: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/html/?q="),
    ("startpage", "Startpage", "https://www.startpage.com/sp/search?query="),
    ("brave", "Brave", "https://search.brave.com/search?q="),
    ("google", "Google", "https://www.google.com/search?q="),
];

/// The languages the interface is offered in: `(key, name in that language)`.
///
/// A language is named in its own script, because someone looking for it cannot
/// necessarily read the current one.
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("hi", "हिन्दी")];

/// The zoom steps the menu and Ctrl +/- move between.
pub const ZOOM_STEPS: &[u32] = &[67, 80, 90, 100, 110, 125, 150, 175, 200];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub layout: TabLayout,
    pub rail: Rail,
    /// Index into [`ENGINES`].
    pub engine: usize,
    /// Percent. Applies to newly opened tabs; each tab can then differ.
    pub zoom: u32,
    pub blocking: bool,
    /// Reopen the previous session's tabs at launch.
    pub restore: bool,
    /// Animate the tab rail opening and closing. Off is the reduced-motion
    /// setting: there is no OS-level signal to read here, so it is offered
    /// directly rather than assumed.
    pub motion: bool,
    /// Index into [`LANGUAGES`] — which language the chrome speaks.
    pub language: usize,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            layout: TabLayout::Vertical,
            rail: Rail::Expanded,
            engine: 0,
            zoom: 100,
            blocking: true,
            restore: true,
            motion: true,
            language: 0,
        }
    }
}

impl Settings {
    /// Apply one `key=value` pair. Returns whether anything was recognised, so a
    /// stale link cannot silently do nothing.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        match (key, value) {
            ("layout", "vertical") => self.layout = TabLayout::Vertical,
            ("layout", "horizontal") => self.layout = TabLayout::Horizontal,
            ("rail", "expanded") => self.rail = Rail::Expanded,
            ("rail", "icons") => self.rail = Rail::Icons,
            ("rail", "hidden") => self.rail = Rail::Hidden,
            ("blocking", v) => self.blocking = v == "on",
            ("restore", v) => self.restore = v == "on",
            ("motion", v) => self.motion = v == "on",
            ("zoom", v) => match v.parse::<u32>() {
                Ok(z) if ZOOM_STEPS.contains(&z) => self.zoom = z,
                _ => return false,
            },
            ("engine", v) => match ENGINES.iter().position(|(key, ..)| *key == v) {
                Some(i) => self.engine = i,
                None => return false,
            },
            ("lang", v) => match LANGUAGES.iter().position(|(key, _)| *key == v) {
                Some(i) => self.language = i,
                None => return false,
            },
            _ => return false,
        }
        true
    }

    /// Apply every pair in a `key=value&key=value` query string.
    pub fn apply_query(&mut self, query: &str) -> bool {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .fold(false, |changed, (k, v)| self.set(k, v) || changed)
    }

    fn serialize(&self) -> String {
        format!(
            "layout={}\nrail={}\nengine={}\nzoom={}\nblocking={}\nrestore={}\nmotion={}\nlang={}\n",
            match self.layout {
                TabLayout::Vertical => "vertical",
                TabLayout::Horizontal => "horizontal",
            },
            match self.rail {
                Rail::Expanded => "expanded",
                Rail::Icons => "icons",
                Rail::Hidden => "hidden",
            },
            ENGINES[self.engine.min(ENGINES.len() - 1)].0,
            self.zoom,
            if self.blocking { "on" } else { "off" },
            if self.restore { "on" } else { "off" },
            if self.motion { "on" } else { "off" },
            LANGUAGES[self.language.min(LANGUAGES.len() - 1)].0,
        )
    }

    /// An unreadable or partly corrupt file falls back to the defaults rather
    /// than refusing to start: a bad preference must never cost you the browser.
    fn read() -> Settings {
        let mut settings = Settings::default();
        let Some(dir) = crate::storage::profile_dir() else { return settings };
        let Some(text) = crate::crypto::read_file(&dir.join("settings.tsv")) else {
            return settings;
        };
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                settings.set(key.trim(), value.trim());
            }
        }
        settings
    }

    fn write(&self) {
        if let Some(dir) = crate::storage::profile_dir() {
            crate::crypto::write_file(&dir.join("settings.tsv"), &self.serialize());
        }
    }

    /// The chosen language's key, e.g. `hi`.
    pub fn language(&self) -> &'static str {
        LANGUAGES[self.language.min(LANGUAGES.len() - 1)].0
    }

    /// The chosen engine's `(key, label, query prefix)`.
    pub fn engine(&self) -> (&'static str, &'static str, &'static str) {
        ENGINES[self.engine.min(ENGINES.len() - 1)]
    }

    /// The search URL for a query, using the chosen engine.
    pub fn search_url(&self, query: &str) -> String {
        format!("{}{}", self.engine().2, zero_engine::percent_encode(query))
    }

    /// The same engine as a GET form's `(action, field name)`, so the new-tab
    /// search box submits exactly where the address bar would send it.
    pub fn search_form(&self) -> (&'static str, &'static str) {
        let prefix = self.engine().2;
        let (action, field) = prefix.split_once('?').unwrap_or((prefix, "q="));
        (action, field.trim_end_matches('=').trim_end_matches('&'))
    }
}

thread_local! {
    /// Read once at startup; the shell is single-threaded, and this keeps
    /// preference lookups (which happen per subresource) off the disk.
    ///
    /// Tests drive the same code paths the UI does, so under `cfg(test)` this
    /// starts from the defaults and never touches the user's real profile.
    static CURRENT: RefCell<Settings> =
        RefCell::new(if cfg!(test) { Settings::default() } else { Settings::read() });
}

pub fn current() -> Settings {
    CURRENT.with(|c| *c.borrow())
}

/// Replace the live settings and persist them.
pub fn store(settings: Settings) {
    if !cfg!(test) {
        settings.write();
    }
    preview(settings);
}

/// Use these settings for the rest of this process without writing them down.
/// Headless screenshots pose the chrome this way, so taking a picture of the
/// browser can never change how it opens next time.
pub fn preview(settings: Settings) {
    CURRENT.with(|c| *c.borrow_mut() = settings);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_pairs_change_only_what_they_name() {
        let mut settings = Settings::default();
        assert!(settings.apply_query("rail=icons&zoom=125"));
        assert_eq!(settings.rail, Rail::Icons);
        assert_eq!(settings.zoom, 125);
        // Untouched fields keep their values.
        assert_eq!(settings.layout, TabLayout::Vertical);
        assert!(settings.blocking);
    }

    #[test]
    fn unknown_and_out_of_range_values_are_refused() {
        let mut settings = Settings::default();
        assert!(!settings.apply_query("rail=sideways"));
        assert!(!settings.apply_query("zoom=13")); // not a step
        assert!(!settings.apply_query("engine=altavista"));
        assert!(!settings.apply_query("nonsense"));
        assert_eq!(settings, Settings::default());
        // One good pair among bad ones still counts as a change.
        assert!(settings.apply_query("bogus=1&zoom=150"));
        assert_eq!(settings.zoom, 150);
    }

    #[test]
    fn settings_round_trip_through_their_own_format() {
        let saved = Settings {
            layout: TabLayout::Horizontal,
            rail: Rail::Hidden,
            engine: 2,
            zoom: 90,
            blocking: false,
            restore: false,
            motion: false,
            language: 1,
        };
        let mut read_back = Settings::default();
        for line in saved.serialize().lines() {
            let (key, value) = line.split_once('=').expect("key=value");
            assert!(read_back.set(key, value), "unreadable line: {line}");
        }
        assert_eq!(read_back, saved);
    }

    #[test]
    fn the_collapse_control_cycles_back_to_where_it_started() {
        assert_eq!(Rail::Expanded.next().next().next(), Rail::Expanded);
    }

    #[test]
    fn the_start_page_form_submits_where_the_address_bar_would() {
        // Every engine's prefix has to split cleanly into a form action and a
        // field name, or the new-tab box would search somewhere else.
        for (index, (key, _, prefix)) in ENGINES.iter().enumerate() {
            let settings = Settings { engine: index, ..Settings::default() };
            let (action, field) = settings.search_form();
            assert!(!field.is_empty() && !field.contains('='), "{key}: bad field {field}");
            assert_eq!(format!("{action}?{field}="), *prefix, "{key} does not round-trip");
        }
        // Startpage is the one that does not call its field "q".
        assert_eq!(Settings { engine: 1, ..Settings::default() }.search_form().1, "query");
    }

    #[test]
    fn search_uses_the_chosen_engine_and_encodes_the_query() {
        let settings = Settings { engine: 3, ..Settings::default() };
        assert_eq!(settings.search_url("rust lang"), "https://www.google.com/search?q=rust+lang");
    }
}
