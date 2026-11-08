//! Edit operations, the same function every surface calls.

use crate::error::Alpha2Error;

/// An edit operation's result as the engine's canonical JSON text, byte for byte what every
/// surface answers.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct EditedProject {
    /// The edited project document.
    pub project_json: String,
    /// What changed: `removed` (old timeline ranges), `moved` (`from` and `to` ranges) and
    /// `created` (clip ids).
    pub report_json: String,
}

/// Applies an edit operation (`remove_ranges`, `split_at`, `bake`) to a project, both JSON text.
///
/// # Errors
/// [`Alpha2Error::EditRefused`] for an invalid project or request and for every refusal;
/// `INTERNAL` for an edited project that fails validation, a defect.
#[uniffi::export]
pub fn apply_edit(project_json: &str, request_json: &str) -> Result<EditedProject, Alpha2Error> {
    let edited = alpha2_eval::edit::apply_json(project_json, request_json).map_err(|failure| {
        if failure.code == "INVALID_INPUT" {
            Alpha2Error::EditRefused {
                refusal: failure.refusal.to_owned(),
                reason: failure.message,
            }
        } else {
            Alpha2Error::Failed {
                code: failure.code.to_owned(),
                reason: failure.message,
            }
        }
    })?;
    Ok(EditedProject {
        project_json: edited.project,
        report_json: edited.report,
    })
}
