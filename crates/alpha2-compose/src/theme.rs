//! Themes: the built-in ones, and a spec's theme laid over its base.

use alpha2_model::Rgba;

use crate::spec::{Theme, ThemeChoice, Transition};

/// The built-in themes, by name.
pub const THEMES: [&str; 2] = ["clean-dark", "clean-light"];

/// A theme with every part decided.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub font_family: Option<String>,
    pub text: Rgba,
    pub background: Rgba,
    pub surface: Rgba,
    pub panel: Rgba,
    pub accent: Rgba,
    pub title_size: f32,
    pub subtitle_size: f32,
    pub body_size: f32,
    pub caption_size: f32,
    pub label_size: f32,
    pub margin: f32,
    pub transition: Transition,
    pub still_seconds: f64,
    pub text_seconds: f64,
    pub fade_seconds: f64,
}

const fn gray(level: f32, alpha: f32) -> Rgba {
    Rgba::new(level, level, level, alpha)
}

fn builtin(name: &str) -> Option<Resolved> {
    let crossfade = Transition {
        kind: "crossfade".to_owned(),
        duration: 0.5,
        easing: None,
        params: std::collections::BTreeMap::new(),
        version: None,
    };
    let common = Resolved {
        font_family: None,
        text: gray(1.0, 1.0),
        background: gray(0.0, 1.0),
        surface: Rgba::new(0.067, 0.075, 0.094, 1.0),
        panel: gray(0.0, 0.6),
        accent: Rgba::new(0.31, 0.545, 1.0, 1.0),
        title_size: 0.1,
        subtitle_size: 0.045,
        body_size: 0.04,
        caption_size: 0.06,
        label_size: 0.034,
        margin: 0.04,
        transition: crossfade,
        still_seconds: 4.0,
        text_seconds: 3.0,
        fade_seconds: 0.4,
    };
    match name {
        "clean-dark" => Some(common),
        "clean-light" => Some(Resolved {
            text: gray(0.067, 1.0),
            background: gray(1.0, 1.0),
            surface: Rgba::new(0.96, 0.96, 0.95, 1.0),
            panel: gray(1.0, 0.75),
            accent: Rgba::new(0.0, 0.4, 0.85, 1.0),
            ..common
        }),
        _ => None,
    }
}

/// The theme `choice` names or builds, or why it is not one.
pub(crate) fn resolve(choice: &ThemeChoice) -> Result<Resolved, String> {
    let unknown = |name: &str| {
        format!(
            "unknown theme \"{name}\"; the built-in themes are {}",
            THEMES.join(", ")
        )
    };
    let theme: &Theme = match choice {
        ThemeChoice::Named(name) => return builtin(name).ok_or_else(|| unknown(name)),
        ThemeChoice::Custom(theme) => theme,
    };
    let base_name = theme.base.as_deref().unwrap_or(THEMES[0]);
    let base = builtin(base_name).ok_or_else(|| unknown(base_name))?;
    let size = |value: Option<f32>, fallback: f32, field: &str| match value {
        Some(value) if !(value.is_finite() && value > 0.0 && value <= 1.0) => Err(format!(
            "{field} is {value}; a theme size is a fraction of the canvas's shorter side, in (0, 1]"
        )),
        value => Ok(value.unwrap_or(fallback)),
    };
    let seconds = |value: Option<f64>, fallback: f64, field: &str| match value {
        Some(value) if !(value.is_finite() && value >= 0.0) => {
            Err(format!("{field} is {value}; it is non-negative seconds"))
        }
        value => Ok(value.unwrap_or(fallback)),
    };
    Ok(Resolved {
        font_family: theme.font_family.clone().or(base.font_family),
        text: theme.text.unwrap_or(base.text),
        background: theme.background.unwrap_or(base.background),
        surface: theme.surface.unwrap_or(base.surface),
        panel: theme.panel.unwrap_or(base.panel),
        accent: theme.accent.unwrap_or(base.accent),
        title_size: size(theme.title_size, base.title_size, "title_size")?,
        subtitle_size: size(theme.subtitle_size, base.subtitle_size, "subtitle_size")?,
        body_size: size(theme.body_size, base.body_size, "body_size")?,
        caption_size: size(theme.caption_size, base.caption_size, "caption_size")?,
        label_size: size(theme.label_size, base.label_size, "label_size")?,
        margin: size(theme.margin, base.margin, "margin")?,
        transition: theme.transition.clone().unwrap_or(base.transition),
        still_seconds: seconds(theme.still_seconds, base.still_seconds, "still_seconds")?,
        text_seconds: seconds(theme.text_seconds, base.text_seconds, "text_seconds")?,
        fade_seconds: seconds(theme.fade_seconds, base.fade_seconds, "fade_seconds")?,
    })
}
