//! The operations over JSON text, which every binding calls, so each answers the same bytes.

use alpha2_model::{Project, ProjectWriteError};

use super::{EditError, EditRequest, apply};

/// An operation's result as canonical JSON text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditedJson {
    /// The edited project document.
    pub project: String,
    /// The [`super::EditReport`].
    pub report: String,
}

/// Why [`apply_json`] gave no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditFailure {
    /// `INVALID_INPUT`, or `INTERNAL` for an edited project that fails validation.
    pub code: &'static str,
    /// The reason in snake case, `invalid_project`, `invalid_request`, `result_too_large` or an
    /// [`EditError::kind`].
    pub refusal: &'static str,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for EditFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EditFailure {}

impl From<EditError> for EditFailure {
    fn from(error: EditError) -> Self {
        Self {
            code: if matches!(error, EditError::InvalidResult(_)) {
                "INTERNAL"
            } else {
                "INVALID_INPUT"
            },
            refusal: error.kind(),
            message: error.to_string(),
        }
    }
}

/// Applies the request `request_json` to the project `project_json`.
///
/// # Errors
/// [`EditFailure`] for an invalid project or request, for every [`EditError`], and for an edited
/// project longer than a project document may be (`result_too_large`).
pub fn apply_json(project_json: &str, request_json: &str) -> Result<EditedJson, EditFailure> {
    let project = Project::from_json(project_json).map_err(|error| EditFailure {
        code: "INVALID_INPUT",
        refusal: "invalid_project",
        message: format!("invalid project: {error}"),
    })?;
    let request: EditRequest = serde_json::from_str(request_json).map_err(|error| EditFailure {
        code: "INVALID_INPUT",
        refusal: "invalid_request",
        message: format!("invalid edit request: {error}"),
    })?;
    let (edited, report) = apply(&project, &request)?;
    let text = |value: Result<String, serde_json::Error>| {
        value.map_err(|error| EditFailure {
            code: "INTERNAL",
            refusal: "invalid_result",
            message: error.to_string(),
        })
    };
    let project = edited.to_json().map_err(|error| match error {
        ProjectWriteError::TooLarge { .. } => EditFailure {
            code: "INVALID_INPUT",
            refusal: "result_too_large",
            message: format!("the edit is refused: {error}"),
        },
        error => EditFailure {
            code: "INTERNAL",
            refusal: "invalid_result",
            message: error.to_string(),
        },
    })?;
    Ok(EditedJson {
        project,
        report: text(serde_json::to_string(&report))?,
    })
}
