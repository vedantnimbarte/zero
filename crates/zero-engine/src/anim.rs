//! CSS transitions: a property that changed, arriving over time instead of at once.
//!
//! The engine has no clock of its own — it renders when the embedder asks it to
//! — so time arrives from outside, and an embedder that never supplies one gets
//! the settled state, which is exactly what a screenshot wants.
//!
//! The mechanism is one comparison: every render, an element's newly computed
//! value is compared with what it had last time. If it changed and the element
//! declares a transition for that property, the old value is remembered and the
//! rendered value walks from old to new over the declared duration. Nothing
//! needs to know *why* it changed — `:hover`, a class a script set, a media
//! query flipping at a new window width all animate for free.
//!
//! ponytail: linear easing only, no `transition-delay`, and only properties that
//! interpolate as a single number or colour. Cubic-bezier easing is a curve
//! applied to `progress` when a page needs it; the rest of this stays as is.

use crate::css::{Color, Unit, Value};
use crate::style::PropertyMap;
use std::collections::HashMap;

/// One property on its way from one value to another.
struct Transit {
    from: Value,
    to: Value,
    /// When it started, on the embedder's clock.
    started: f32,
    duration: f32,
}

/// Everything in flight, plus the previous frame's values that reveal a change.
#[derive(Default)]
pub struct Animator {
    /// The embedder's clock, in milliseconds. Monotonic is all that is required.
    now: f32,
    previous: HashMap<(usize, String), Value>,
    running: HashMap<(usize, String), Transit>,
    /// Whether anything moved this frame, so the embedder knows to ask again.
    active: bool,
}

impl Animator {
    /// Advance to `now` (milliseconds). Called once per frame, before styling.
    pub fn set_time(&mut self, now: f32) {
        self.now = now;
        self.active = false;
    }

    /// Is anything still in flight? The embedder uses this to decide whether to
    /// draw another frame, which is the only reason an idle page ever stops.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Replace freshly computed values with where they have actually got to.
    ///
    /// Called for each element as it is styled, which is the one moment both the
    /// old and the new values exist.
    pub fn apply(&mut self, node_id: usize, values: &mut PropertyMap) {
        // Untracked elements (id 0) would all share one slot and animate each
        // other's values.
        if node_id == 0 {
            return;
        }
        let declared = transitions(values);
        for (property, duration) in &declared {
            let Some(target) = values.get(property).cloned() else { continue };
            let key = (node_id, property.clone());
            let previous = self.previous.insert(key.clone(), target.clone());

            match previous {
                // First sight of this element: nothing to move from.
                None => continue,
                Some(previous) if previous == target => {}
                Some(previous) => {
                    // Retargeting mid-flight starts from where it has reached,
                    // so a cursor that leaves and returns does not jump.
                    let from = match self.running.get(&key) {
                        Some(transit) => interpolate(&transit.from, &transit.to, self.progress(transit)),
                        None => Some(previous),
                    };
                    if let Some(from) = from {
                        self.running.insert(
                            key.clone(),
                            Transit { from, to: target.clone(), started: self.now, duration: *duration },
                        );
                    }
                }
            }

            let Some(transit) = self.running.get(&key) else { continue };
            let progress = self.progress(transit);
            if progress >= 1.0 {
                self.running.remove(&key);
                continue;
            }
            if let Some(value) = interpolate(&transit.from, &transit.to, progress) {
                values.insert(property.clone(), value);
                self.active = true;
            }
        }
        // A property that stopped being transitioned still has to be remembered,
        // or turning a transition back on would animate from a stale value.
        if declared.is_empty() {
            self.previous.retain(|(id, _), _| *id != node_id);
        }
    }

    fn progress(&self, transit: &Transit) -> f32 {
        match transit.duration > 0.0 {
            true => ((self.now - transit.started) / transit.duration).clamp(0.0, 1.0),
            false => 1.0,
        }
    }
}

/// The properties this element transitions, and for how long.
///
/// `transition: color 300ms, background 1s` and the longhand pair
/// (`transition-property` + `transition-duration`) both land here.
fn transitions(values: &PropertyMap) -> Vec<(String, f32)> {
    if let (Some(Value::Raw(names)), Some(duration)) = (
        values.get("transition-property"),
        values.get("transition-duration"),
    ) {
        let seconds = duration_of(&raw_text(duration));
        return names
            .split(',')
            .map(|name| (name.trim().to_string(), seconds))
            .filter(|(name, _)| !name.is_empty())
            .collect();
    }
    let Some(Value::Raw(shorthand)) = values.get("transition") else { return Vec::new() };
    shorthand
        .split(',')
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            let property = words.next()?.to_string();
            // `transition: 300ms color` is as legal as the other order, so the
            // duration is whichever word looks like a time.
            let duration = part.split_whitespace().find_map(|word| {
                let ms = duration_of(word);
                (ms > 0.0).then_some(ms)
            })?;
            match property == "all" || duration_of(&property) > 0.0 {
                // `all` would need every property compared every frame; the
                // named form is what pages actually write.
                true => None,
                false => Some((property, duration)),
            }
        })
        .collect()
}

fn raw_text(value: &Value) -> String {
    match value {
        Value::Raw(text) => text.clone(),
        Value::Length(v, Unit::Px) => format!("{v}px"),
        other => format!("{other:?}"),
    }
}

/// `300ms`, `.3s`, `1s` — in milliseconds. Anything else is not a duration.
fn duration_of(word: &str) -> f32 {
    let word = word.trim();
    if let Some(ms) = word.strip_suffix("ms") {
        return ms.parse().unwrap_or(0.0);
    }
    if let Some(s) = word.strip_suffix('s') {
        return s.parse::<f32>().unwrap_or(0.0) * 1000.0;
    }
    0.0
}

/// Where a property has got to, `0.0` being `from` and `1.0` being `to`.
///
/// Values that cannot be interpolated (keywords like `none`) return `None`, and
/// the property simply snaps — which is what CSS does with them too.
fn interpolate(from: &Value, to: &Value, progress: f32) -> Option<Value> {
    let mix = |a: f32, b: f32| a + (b - a) * progress;
    match (from, to) {
        (Value::ColorValue(a), Value::ColorValue(b)) => {
            let channel = |a: u8, b: u8| mix(a as f32, b as f32).round().clamp(0.0, 255.0) as u8;
            Some(Value::ColorValue(Color {
                r: channel(a.r, b.r),
                g: channel(a.g, b.g),
                b: channel(a.b, b.b),
                a: channel(a.a, b.a),
            }))
        }
        (Value::Number(a), Value::Number(b)) => Some(Value::Number(mix(*a, *b))),
        // Two lengths only interpolate in the same unit; `0` to `100%` would
        // need the containing block, which styling does not have here.
        (Value::Length(a, unit_a), Value::Length(b, unit_b)) if unit_a == unit_b => {
            Some(Value::Length(mix(*a, *b), *unit_a))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Value {
        Value::ColorValue(Color { r: 255, g: 0, b: 0, a: 255 })
    }

    fn blue() -> Value {
        Value::ColorValue(Color { r: 0, g: 0, b: 255, a: 255 })
    }

    fn values(color: Value) -> PropertyMap {
        PropertyMap::from([
            ("color".to_string(), color),
            ("transition".to_string(), Value::Raw("color 400ms".into())),
        ])
    }

    #[test]
    fn a_changed_property_crosses_over_the_declared_time() {
        let mut anim = Animator::default();

        // First render: nothing to move from, and nothing is animating.
        anim.set_time(0.0);
        let mut first = values(red());
        anim.apply(7, &mut first);
        assert_eq!(first["color"], red());
        assert!(!anim.is_active());

        // The value changes: this frame still shows the old one.
        anim.set_time(0.0);
        let mut changed = values(blue());
        anim.apply(7, &mut changed);
        assert_eq!(changed["color"], red());
        assert!(anim.is_active(), "the embedder must know to draw again");

        // Halfway: halfway between the two.
        anim.set_time(200.0);
        let mut midway = values(blue());
        anim.apply(7, &mut midway);
        assert_eq!(
            midway["color"],
            Value::ColorValue(Color { r: 128, g: 0, b: 128, a: 255 })
        );

        // Past the end: the target, and nothing left in flight.
        anim.set_time(400.0);
        let mut done = values(blue());
        anim.apply(7, &mut done);
        assert_eq!(done["color"], blue());
        assert!(!anim.is_active());
    }

    #[test]
    fn elements_do_not_animate_each_others_values() {
        let mut anim = Animator::default();
        anim.set_time(0.0);
        anim.apply(1, &mut values(red()));
        anim.apply(2, &mut values(blue()));

        // Element 2 was blue last frame and is blue now: nothing moves.
        anim.set_time(10.0);
        let mut second = values(blue());
        anim.apply(2, &mut second);
        assert_eq!(second["color"], blue());
        assert!(!anim.is_active());
    }

    #[test]
    fn declarations_are_read_in_either_spelling() {
        assert_eq!(
            transitions(&PropertyMap::from([(
                "transition".to_string(),
                Value::Raw("color 300ms, opacity 1s".into())
            )])),
            vec![("color".to_string(), 300.0), ("opacity".to_string(), 1000.0)]
        );
        // The longhand pair.
        assert_eq!(
            transitions(&PropertyMap::from([
                ("transition-property".to_string(), Value::Raw("width".into())),
                ("transition-duration".to_string(), Value::Raw(".25s".into())),
            ])),
            vec![("width".to_string(), 250.0)]
        );
        // `all` is refused rather than half-honoured, and a bare duration is
        // not a property.
        assert!(transitions(&PropertyMap::from([(
            "transition".to_string(),
            Value::Raw("all 200ms".into())
        )]))
        .is_empty());
    }

    #[test]
    fn only_values_that_can_be_mixed_are() {
        assert_eq!(interpolate(&Value::Number(0.0), &Value::Number(1.0), 0.25), Some(Value::Number(0.25)));
        assert_eq!(
            interpolate(&Value::Length(0.0, Unit::Px), &Value::Length(10.0, Unit::Px), 0.5),
            Some(Value::Length(5.0, Unit::Px))
        );
        // Different units, and keywords, have no midpoint — so they snap.
        assert_eq!(
            interpolate(&Value::Length(0.0, Unit::Px), &Value::Length(10.0, Unit::Percent), 0.5),
            None
        );
        assert_eq!(
            interpolate(&Value::Keyword("none".into()), &Value::Keyword("block".into()), 0.5),
            None
        );
    }
}
