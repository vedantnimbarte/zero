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
//! `@keyframes` animations run off the same clock and the same interpolation.
//! The difference is only where the two ends come from: a transition's `from` is
//! whatever the element had last frame, an animation's are two stops of a named
//! rule, and the progress between them is a function of elapsed time rather than
//! of a change having happened.
//!
//! ponytail: no `transition-delay`, and only properties that interpolate as a
//! single number or colour — a keyword (`display: none`) snaps, which is what
//! CSS does with it too.

use crate::css::{Color, Keyframes, Unit, Value};
use crate::style::PropertyMap;
use std::collections::HashMap;

/// A CSS easing function: what a fraction of the elapsed time means for how far
/// along the value is.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Easing {
    #[default]
    Linear,
    /// `cubic-bezier(x1, y1, x2, y2)`, which `ease` and friends are named
    /// shorthands for.
    Bezier(f32, f32, f32, f32),
    /// `steps(n, jump-start | jump-end)` — `true` jumps at the start.
    Steps(u32, bool),
}

impl Easing {
    /// Parse one easing keyword or function. Anything unrecognised is linear,
    /// which is the honest answer for a curve we cannot draw.
    pub fn parse(text: &str) -> Easing {
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        match lower.as_str() {
            // The named curves, with the control points the spec gives them.
            "ease" => return Easing::Bezier(0.25, 0.1, 0.25, 1.0),
            "ease-in" => return Easing::Bezier(0.42, 0.0, 1.0, 1.0),
            "ease-out" => return Easing::Bezier(0.0, 0.0, 0.58, 1.0),
            "ease-in-out" => return Easing::Bezier(0.42, 0.0, 0.58, 1.0),
            "step-start" => return Easing::Steps(1, true),
            "step-end" => return Easing::Steps(1, false),
            _ => {}
        }
        if let Some(args) = lower
            .strip_prefix("cubic-bezier(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let n: Vec<f32> = args
                .split(',')
                .filter_map(|a| a.trim().parse::<f32>().ok())
                .collect();
            if let [x1, y1, x2, y2] = n[..] {
                return Easing::Bezier(x1, y1, x2, y2);
            }
        }
        if let Some(args) = lower
            .strip_prefix("steps(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let mut parts = args.split(',');
            let count = parts.next().and_then(|n| n.trim().parse::<u32>().ok());
            let position = parts.next().map(str::trim).unwrap_or("jump-end");
            if let Some(count) = count.filter(|n| *n > 0) {
                return Easing::Steps(count, matches!(position, "jump-start" | "start"));
            }
        }
        Easing::Linear
    }

    /// How far along the value is, given how far along the time is.
    pub fn at(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::Steps(count, jump_start) => {
                let steps = count as f32;
                let step = match jump_start {
                    true => (t * steps).floor() + 1.0,
                    false => (t * steps).floor(),
                };
                (step / steps).clamp(0.0, 1.0)
            }
            // A cubic Bezier's y for a given x. x(t) has no closed-form
            // inverse, so it is found by bisection — a dozen halvings is well
            // inside a pixel and needs no derivative.
            Easing::Bezier(x1, y1, x2, y2) => {
                let curve = |p1: f32, p2: f32, u: f32| {
                    let v = 1.0 - u;
                    3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u
                };
                let mut low = 0.0;
                let mut high = 1.0;
                let mut u = t;
                for _ in 0..16 {
                    let x = curve(x1, x2, u);
                    if (x - t).abs() < 1.0e-4 {
                        break;
                    }
                    if x < t {
                        low = u;
                    } else {
                        high = u;
                    }
                    u = (low + high) / 2.0;
                }
                curve(y1, y2, u)
            }
        }
    }
}

/// One property on its way from one value to another.
struct Transit {
    from: Value,
    to: Value,
    /// When it started, on the embedder's clock.
    started: f32,
    duration: f32,
    easing: Easing,
}

/// Everything in flight, plus the previous frame's values that reveal a change.
pub struct Animator {
    /// The embedder's clock, in milliseconds. Monotonic is all that is required.
    now: f32,
    previous: HashMap<(usize, String), Value>,
    running: HashMap<(usize, String), Transit>,
    /// Whether anything moved this frame, so the embedder knows to ask again.
    active: bool,
    /// When an animation was first seen on an element, so elapsed time is
    /// measured from when it started rather than from when the page loaded.
    started: HashMap<(usize, String), f32>,
    /// `Animation: Off`. Animations hold at their fill state and transitions
    /// snap, rather than the clock being frozen mid-cycle.
    enabled: bool,
}

impl Default for Animator {
    fn default() -> Self {
        Animator {
            now: 0.0,
            previous: HashMap::new(),
            running: HashMap::new(),
            active: false,
            started: HashMap::new(),
            enabled: true,
        }
    }
}

impl Animator {
    /// Advance to `now` (milliseconds). Called once per frame, before styling.
    pub fn set_time(&mut self, now: f32) {
        self.now = now;
        self.active = false;
    }

    /// Turn page animation on or off, following the embedder's reduced-motion
    /// setting. Off is not a frozen clock: each animation holds at the state
    /// its `fill-mode` says it ends in, so a page that fades in is *shown*
    /// rather than left invisible.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
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
    pub fn apply(&mut self, node_id: usize, values: &mut PropertyMap, keyframes: &[Keyframes]) {
        // Untracked elements (id 0) would all share one slot and animate each
        // other's values.
        if node_id == 0 {
            return;
        }
        // Animations run first: a transition compares against the last frame's
        // value, and an animated property's last frame was also animated.
        self.apply_animations(node_id, values, keyframes);
        let declared = transitions(values);
        for (property, duration) in &declared {
            let Some(target) = values.get(property).cloned() else {
                continue;
            };
            // With motion off a transition has no midpoint to show: the new
            // value is simply the value.
            if !self.enabled {
                self.previous.insert((node_id, property.clone()), target);
                continue;
            }
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
                        Some(transit) => {
                            interpolate(&transit.from, &transit.to, self.progress(transit))
                        }
                        None => Some(previous),
                    };
                    if let Some(from) = from {
                        self.running.insert(
                            key.clone(),
                            Transit {
                                from,
                                to: target.clone(),
                                started: self.now,
                                duration: *duration,
                                easing: easing_of(values, "transition-timing-function"),
                            },
                        );
                    }
                }
            }

            let Some(transit) = self.running.get(&key) else {
                continue;
            };
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
        let elapsed = match transit.duration > 0.0 {
            true => ((self.now - transit.started) / transit.duration).clamp(0.0, 1.0),
            false => 1.0,
        };
        transit.easing.at(elapsed)
    }

    /// Override this element's computed values with wherever its `@keyframes`
    /// animations have got to.
    fn apply_animations(
        &mut self,
        node_id: usize,
        values: &mut PropertyMap,
        keyframes: &[Keyframes],
    ) {
        for animation in animations(values) {
            let Some(rule) = keyframes.iter().find(|k| k.name == animation.name) else {
                continue; // a name with no rule animates nothing
            };
            let key = (node_id, animation.name.clone());
            let started = *self.started.entry(key).or_insert(self.now);
            let Some(progress) = animation.progress(self.now - started, self.enabled) else {
                continue; // not started yet, or finished with no fill
            };
            // Every property any stop mentions, at this moment.
            for (property, value) in sample(rule, progress) {
                values.insert(property, value);
            }
            if self.enabled && !animation.is_finished(self.now - started) {
                self.active = true;
            }
        }
    }
}

/// One `animation` in a page's list, with every longhand resolved.
#[derive(Debug, PartialEq)]
struct Animation {
    name: String,
    duration: f32,
    delay: f32,
    easing: Easing,
    /// `f32::INFINITY` for `infinite`.
    iterations: f32,
    direction: Direction,
    fill: Fill,
    paused: bool,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Fill {
    None,
    Forwards,
    Backwards,
    Both,
}

impl Animation {
    /// Whether the animation has run out of iterations.
    fn is_finished(&self, elapsed: f32) -> bool {
        self.iterations.is_finite() && elapsed >= self.delay + self.duration * self.iterations
    }

    /// How far through its keyframes this animation is, or `None` when it is
    /// not affecting the element at all.
    ///
    /// `enabled` false is `Animation: Off`: the animation holds at the state it
    /// finishes in rather than at wherever the clock happens to be.
    fn progress(&self, elapsed: f32, enabled: bool) -> Option<f32> {
        if self.duration <= 0.0 || self.iterations <= 0.0 {
            return None;
        }
        if !enabled {
            return Some(self.eased(self.at_end()));
        }
        if elapsed < self.delay {
            // Before it starts, only a backwards fill has anything to say.
            return matches!(self.fill, Fill::Backwards | Fill::Both)
                .then(|| self.eased(self.first_fraction()));
        }
        let running = elapsed - self.delay;
        let iteration = (running / self.duration).floor();
        if iteration >= self.iterations {
            // After it ends, only a forwards fill holds the last frame.
            return matches!(self.fill, Fill::Forwards | Fill::Both)
                .then(|| self.eased(self.at_end()));
        }
        // A paused animation sits at its current position rather than vanishing.
        let fraction = match self.paused {
            true => 0.0,
            false => (running / self.duration).fract(),
        };
        Some(self.eased(self.oriented(fraction, iteration as u32)))
    }

    /// The fraction the first frame shows, which `direction` may flip.
    fn first_fraction(&self) -> f32 {
        self.oriented(0.0, 0)
    }

    /// The fraction the last frame shows.
    fn at_end(&self) -> f32 {
        let last = if self.iterations.is_finite() {
            (self.iterations.ceil() as u32).saturating_sub(1)
        } else {
            0
        };
        self.oriented(1.0, last)
    }

    /// `animation-direction` applied to one iteration's fraction.
    fn oriented(&self, fraction: f32, iteration: u32) -> f32 {
        let reverse = match self.direction {
            Direction::Normal => false,
            Direction::Reverse => true,
            Direction::Alternate => iteration % 2 == 1,
            Direction::AlternateReverse => iteration.is_multiple_of(2),
        };
        match reverse {
            true => 1.0 - fraction,
            false => fraction,
        }
    }

    /// ponytail: the easing is applied to the whole animation's fraction rather
    /// than per keyframe interval. For the two-stop animations pages actually
    /// write these are the same thing; a multi-stop rule with a non-linear
    /// curve eases across the whole run instead of within each segment.
    fn eased(&self, fraction: f32) -> f32 {
        self.easing.at(fraction)
    }
}

/// Every property any stop of `rule` mentions, interpolated at `progress`.
fn sample(rule: &Keyframes, progress: f32) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let names: Vec<&String> = {
        let mut names: Vec<&String> = rule
            .stops
            .iter()
            .flat_map(|(_, decls)| decls.iter().map(|d| &d.name))
            .collect();
        names.dedup();
        names
    };
    for name in names {
        // The stops that mention this property, which need not be all of them.
        let mut mentioning = rule
            .stops
            .iter()
            .filter_map(|(at, decls)| {
                decls
                    .iter()
                    .find(|d| &d.name == name)
                    .map(|d| (*at, &d.value))
            })
            .peekable();
        let first = match mentioning.peek() {
            Some(first) => *first,
            None => continue,
        };
        let mut before = first;
        let mut after = first;
        for stop in mentioning {
            if stop.0 <= progress {
                before = stop;
            }
            after = stop;
            if stop.0 >= progress {
                break;
            }
        }
        let span = after.0 - before.0;
        let local = match span > 0.0 {
            true => ((progress - before.0) / span).clamp(0.0, 1.0),
            false => 0.0,
        };
        let value = interpolate(before.1, after.1, local)
            // A value with no midpoint snaps at the halfway mark, as CSS does.
            .unwrap_or_else(|| match local < 0.5 {
                true => before.1.clone(),
                false => after.1.clone(),
            });
        out.push((name.clone(), value));
    }
    out
}

/// The animations this element declares, shorthand or longhands.
fn animations(values: &PropertyMap) -> Vec<Animation> {
    // The longhands, each a comma-separated list read in parallel.
    let list = |name: &str| -> Vec<String> {
        match values.get(name) {
            Some(value) => raw_text(value)
                .split(',')
                .map(|part| part.trim().to_string())
                .collect(),
            None => Vec::new(),
        }
    };
    let names = list("animation-name");
    if !names.is_empty() && names.iter().any(|n| !n.is_empty() && n != "none") {
        let durations = list("animation-duration");
        let easings = list("animation-timing-function");
        let delays = list("animation-delay");
        let counts = list("animation-iteration-count");
        let directions = list("animation-direction");
        let fills = list("animation-fill-mode");
        let states = list("animation-play-state");
        // A list shorter than the names repeats, per spec.
        let nth = |list: &[String], i: usize| -> String {
            match list.is_empty() {
                true => String::new(),
                false => list[i % list.len()].clone(),
            }
        };
        return names
            .iter()
            .enumerate()
            .filter(|(_, name)| !name.is_empty() && *name != "none")
            .map(|(i, name)| Animation {
                name: name.clone(),
                duration: duration_of(&nth(&durations, i)),
                delay: duration_of(&nth(&delays, i)),
                easing: Easing::parse(&nth(&easings, i)),
                iterations: iteration_count(&nth(&counts, i)),
                direction: direction_of(&nth(&directions, i)),
                fill: fill_of(&nth(&fills, i)),
                paused: nth(&states, i) == "paused",
            })
            .collect();
    }
    let Some(shorthand) = values.get("animation").map(raw_text) else {
        return Vec::new();
    };
    shorthand
        .split(',')
        .filter_map(parse_animation_shorthand)
        .collect()
}

/// `animation: spin 1s linear 0.5s infinite alternate both`.
///
/// The order within one animation is free apart from the two times, of which
/// the first is the duration and the second the delay — which is the one thing
/// a naive keyword scan gets wrong.
fn parse_animation_shorthand(spec: &str) -> Option<Animation> {
    let mut name = None;
    let mut times = Vec::new();
    let mut easing = Easing::Linear;
    let mut iterations = 1.0;
    let mut direction = Direction::Normal;
    let mut fill = Fill::None;
    let mut paused = false;
    for token in split_functions(spec) {
        let lower = token.to_ascii_lowercase();
        if duration_of(&lower) != 0.0 || lower == "0s" || lower == "0ms" {
            times.push(duration_of(&lower));
            continue;
        }
        match lower.as_str() {
            "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" | "step-start"
            | "step-end" => easing = Easing::parse(&lower),
            _ if lower.starts_with("cubic-bezier(") || lower.starts_with("steps(") => {
                easing = Easing::parse(&lower)
            }
            "infinite" => iterations = f32::INFINITY,
            "normal" => {}
            "reverse" => direction = Direction::Reverse,
            "alternate" => direction = Direction::Alternate,
            "alternate-reverse" => direction = Direction::AlternateReverse,
            "none" => fill = Fill::None,
            "forwards" => fill = Fill::Forwards,
            "backwards" => fill = Fill::Backwards,
            "both" => fill = Fill::Both,
            "running" => paused = false,
            "paused" => paused = true,
            _ => match lower.parse::<f32>() {
                Ok(count) => iterations = count,
                // Whatever is left is the keyframes name, in its original case.
                Err(_) => name = Some(token.to_string()),
            },
        }
    }
    Some(Animation {
        name: name?,
        duration: times.first().copied().unwrap_or(0.0),
        delay: times.get(1).copied().unwrap_or(0.0),
        easing,
        iterations,
        direction,
        fill,
        paused,
    })
}

/// Split on whitespace, keeping `cubic-bezier(.4, 0, .2, 1)` in one piece.
fn split_functions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn iteration_count(text: &str) -> f32 {
    match text.trim() {
        "infinite" => f32::INFINITY,
        "" => 1.0,
        other => other.parse().unwrap_or(1.0),
    }
}

fn direction_of(text: &str) -> Direction {
    match text.trim() {
        "reverse" => Direction::Reverse,
        "alternate" => Direction::Alternate,
        "alternate-reverse" => Direction::AlternateReverse,
        _ => Direction::Normal,
    }
}

fn fill_of(text: &str) -> Fill {
    match text.trim() {
        "forwards" => Fill::Forwards,
        "backwards" => Fill::Backwards,
        "both" => Fill::Both,
        _ => Fill::None,
    }
}

/// The easing a property's `*-timing-function` names, or linear.
fn easing_of(values: &PropertyMap, property: &str) -> Easing {
    match values.get(property) {
        Some(value) => Easing::parse(&raw_text(value)),
        None => Easing::Linear,
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
    let Some(Value::Raw(shorthand)) = values.get("transition") else {
        return Vec::new();
    };
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
        Value::ColorValue(Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        })
    }

    fn blue() -> Value {
        Value::ColorValue(Color {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        })
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
        anim.apply(7, &mut first, &[]);
        assert_eq!(first["color"], red());
        assert!(!anim.is_active());

        // The value changes: this frame still shows the old one.
        anim.set_time(0.0);
        let mut changed = values(blue());
        anim.apply(7, &mut changed, &[]);
        assert_eq!(changed["color"], red());
        assert!(anim.is_active(), "the embedder must know to draw again");

        // Halfway: halfway between the two.
        anim.set_time(200.0);
        let mut midway = values(blue());
        anim.apply(7, &mut midway, &[]);
        assert_eq!(
            midway["color"],
            Value::ColorValue(Color {
                r: 128,
                g: 0,
                b: 128,
                a: 255
            })
        );

        // Past the end: the target, and nothing left in flight.
        anim.set_time(400.0);
        let mut done = values(blue());
        anim.apply(7, &mut done, &[]);
        assert_eq!(done["color"], blue());
        assert!(!anim.is_active());
    }

    #[test]
    fn elements_do_not_animate_each_others_values() {
        let mut anim = Animator::default();
        anim.set_time(0.0);
        anim.apply(1, &mut values(red()), &[]);
        anim.apply(2, &mut values(blue()), &[]);

        // Element 2 was blue last frame and is blue now: nothing moves.
        anim.set_time(10.0);
        let mut second = values(blue());
        anim.apply(2, &mut second, &[]);
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
            vec![
                ("color".to_string(), 300.0),
                ("opacity".to_string(), 1000.0)
            ]
        );
        // The longhand pair.
        assert_eq!(
            transitions(&PropertyMap::from([
                (
                    "transition-property".to_string(),
                    Value::Raw("width".into())
                ),
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

    /// `@keyframes spin { from { opacity: 0 } to { opacity: 1 } }`, parsed.
    fn spin() -> Vec<Keyframes> {
        crate::css::parse("@keyframes spin { from { opacity: 0; } to { opacity: 1; } }".to_string())
            .keyframes
    }

    fn animated(spec: &str) -> PropertyMap {
        PropertyMap::from([
            ("opacity".to_string(), Value::Number(1.0)),
            ("animation".to_string(), Value::Raw(spec.to_string())),
        ])
    }

    #[test]
    fn keyframes_parse_with_percentage_stops_and_from_to() {
        let sheet = crate::css::parse(
            "@keyframes pulse { from { opacity: 1; } 50% { opacity: 0.4; } to { opacity: 1; } } \
             @keyframes pulse { 0%, 100% { opacity: 0.5; } }"
                .to_string(),
        );
        // Redefinition: the last rule of a name is the only one left.
        assert_eq!(sheet.keyframes.len(), 1);
        let rule = &sheet.keyframes[0];
        assert_eq!(rule.name, "pulse");
        // One stop listing two positions becomes two stops, sorted.
        assert_eq!(rule.stops.len(), 2);
        assert_eq!(rule.stops[0].0, 0.0);
        assert_eq!(rule.stops[1].0, 1.0);

        let sheet = crate::css::parse(
            "@keyframes p { from { opacity: 1; } 50% { opacity: 0; } }".to_string(),
        );
        let stops: Vec<f32> = sheet.keyframes[0].stops.iter().map(|(at, _)| *at).collect();
        assert_eq!(stops, vec![0.0, 0.5]);
    }

    #[test]
    fn an_animation_walks_its_keyframes_off_the_clock() {
        let keyframes = spin();
        let mut anim = Animator::default();

        anim.set_time(0.0);
        let mut values = animated("spin 1s linear");
        anim.apply(9, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(0.0));
        assert!(anim.is_active(), "a running animation must ask for a frame");

        anim.set_time(500.0);
        let mut values = animated("spin 1s linear");
        anim.apply(9, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(0.5));

        // Past the end, with no fill-mode, the cascade's own value stands again.
        anim.set_time(2000.0);
        let mut values = animated("spin 1s linear");
        anim.apply(9, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(1.0));
        assert!(
            !anim.is_active(),
            "a finished animation still asked for frames"
        );
    }

    #[test]
    fn infinite_alternate_and_fill_mode_behave() {
        let keyframes = spin();
        let mut anim = Animator::default();
        // `infinite` never finishes, so it never stops asking for frames.
        anim.set_time(0.0);
        anim.apply(
            3,
            &mut animated("spin 1s linear infinite alternate"),
            &keyframes,
        );
        anim.set_time(1_000_000.0);
        let mut values = animated("spin 1s linear infinite alternate");
        anim.apply(3, &mut values, &keyframes);
        assert!(anim.is_active());

        // `alternate` runs the second iteration backwards: a quarter into it is
        // three quarters of the way back.
        let mut anim = Animator::default();
        anim.set_time(0.0);
        anim.apply(
            4,
            &mut animated("spin 1s linear infinite alternate"),
            &keyframes,
        );
        anim.set_time(1250.0);
        let mut values = animated("spin 1s linear infinite alternate");
        anim.apply(4, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(0.75));

        // `forwards` holds the last frame instead of snapping back.
        let mut anim = Animator::default();
        anim.set_time(0.0);
        anim.apply(5, &mut animated("spin 1s linear forwards"), &keyframes);
        anim.set_time(5000.0);
        let mut values = animated("spin 1s linear forwards");
        anim.apply(5, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(1.0));

        // `backwards` shows the first frame during the delay.
        let mut anim = Animator::default();
        anim.set_time(0.0);
        let mut values = animated("spin 1s linear 2s backwards");
        anim.apply(6, &mut values, &keyframes);
        assert_eq!(values["opacity"], Value::Number(0.0));
    }

    #[test]
    fn animation_off_holds_the_fill_state_rather_than_freezing() {
        let keyframes = spin();
        let mut anim = Animator::default();
        anim.set_enabled(false);
        anim.set_time(0.0);
        let mut held = animated("spin 1s linear infinite");
        anim.apply(8, &mut held, &keyframes);
        // The end of the animation, not the start: a page that fades in is
        // shown, not left invisible.
        assert_eq!(held["opacity"], Value::Number(1.0));
        assert!(
            !anim.is_active(),
            "motion is off; nothing should ask for frames"
        );

        // A transition with motion off snaps to its target on the frame it
        // changes rather than crossing over.
        let mut anim = Animator::default();
        anim.set_enabled(false);
        anim.set_time(0.0);
        anim.apply(2, &mut values(red()), &[]);
        anim.set_time(0.0);
        let mut changed = values(blue());
        anim.apply(2, &mut changed, &[]);
        assert_eq!(changed["color"], blue());
        assert!(!anim.is_active());
    }

    #[test]
    fn the_animation_shorthand_reads_in_any_order() {
        let one = parse_animation_shorthand("spin 1.5s ease-in-out 0.2s infinite alternate both")
            .expect("parsed");
        assert_eq!(one.name, "spin");
        assert_eq!(one.duration, 1500.0);
        assert_eq!(one.delay, 200.0);
        assert_eq!(one.easing, Easing::Bezier(0.42, 0.0, 0.58, 1.0));
        assert_eq!(one.iterations, f32::INFINITY);
        assert_eq!(one.direction, Direction::Alternate);
        assert_eq!(one.fill, Fill::Both);

        // Keywords in a different order, a count rather than `infinite`, and a
        // function-valued easing with commas in it.
        let two = parse_animation_shorthand("3 cubic-bezier(0.4, 0, 0.2, 1) 300ms slide reverse")
            .expect("parsed");
        assert_eq!(two.name, "slide");
        assert_eq!(two.duration, 300.0);
        assert_eq!(two.iterations, 3.0);
        assert_eq!(two.easing, Easing::Bezier(0.4, 0.0, 0.2, 1.0));
        assert_eq!(two.direction, Direction::Reverse);

        // The longhands, read in parallel.
        let list = animations(&PropertyMap::from([
            (
                "animation-name".to_string(),
                Value::Raw("spin, slide".into()),
            ),
            (
                "animation-duration".to_string(),
                Value::Raw("1s, 2s".into()),
            ),
            // A shorter list repeats.
            (
                "animation-iteration-count".to_string(),
                Value::Raw("infinite".into()),
            ),
        ]));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].duration, 1000.0);
        assert_eq!(list[1].duration, 2000.0);
        assert!(list.iter().all(|a| a.iterations.is_infinite()));
    }

    #[test]
    fn every_easing_function_curves() {
        // Linear is the identity; the named curves are not, but they do pin
        // both ends.
        for easing in [
            Easing::Linear,
            Easing::Bezier(0.42, 0.0, 0.58, 1.0),
            Easing::Bezier(0.25, 0.1, 0.25, 1.0),
        ] {
            assert!(
                easing.at(0.0).abs() < 1.0e-3,
                "{easing:?} did not start at 0"
            );
            assert!(
                (easing.at(1.0) - 1.0).abs() < 1.0e-3,
                "{easing:?} did not end at 1"
            );
        }
        assert_eq!(Easing::Linear.at(0.25), 0.25);
        // ease-in is behind linear in the first half.
        assert!(Easing::parse("ease-in").at(0.25) < 0.25);
        // ease-out is ahead of it.
        assert!(Easing::parse("ease-out").at(0.25) > 0.25);
        // Steps jump rather than slide.
        let steps = Easing::parse("steps(4)");
        assert_eq!(steps.at(0.1), 0.0);
        assert_eq!(steps.at(0.3), 0.25);
        assert_eq!(Easing::parse("steps(4, jump-start)").at(0.1), 0.25);
        // An unreadable function is linear rather than a guess.
        assert_eq!(Easing::parse("wobble(3)"), Easing::Linear);
    }

    #[test]
    fn only_values_that_can_be_mixed_are() {
        assert_eq!(
            interpolate(&Value::Number(0.0), &Value::Number(1.0), 0.25),
            Some(Value::Number(0.25))
        );
        assert_eq!(
            interpolate(
                &Value::Length(0.0, Unit::Px),
                &Value::Length(10.0, Unit::Px),
                0.5
            ),
            Some(Value::Length(5.0, Unit::Px))
        );
        // Different units, and keywords, have no midpoint — so they snap.
        assert_eq!(
            interpolate(
                &Value::Length(0.0, Unit::Px),
                &Value::Length(10.0, Unit::Percent),
                0.5
            ),
            None
        );
        assert_eq!(
            interpolate(
                &Value::Keyword("none".into()),
                &Value::Keyword("block".into()),
                0.5
            ),
            None
        );
    }
}
