//! RFC 9457 "Problem Details for HTTP APIs".
//!
//! Rules this module enforces:
//! * the five standard members are `type`, `title`, `status`, `detail`, `instance`;
//! * `type` defaults to `about:blank`, in which case `title` is the HTTP reason phrase;
//! * extension members may not shadow a standard member;
//! * `detail` is written for the client. Backend errors (database, IMAP, disk) are
//!   never copied into it (RFC 9457 §5): callers log the real error and return a
//!   generic problem.

use std::collections::BTreeMap;

use fanfold_app::{admin::AdminError, error::AppError};
use fanfold_domain::{job::TransitionError, work::UrlError};
use http::{HeaderValue, Response, StatusCode, header};
use serde::Serialize;
use serde_json::{Value, json};

pub const CONTENT_TYPE: &str = "application/problem+json";

const RESERVED: [&str; 5] = ["type", "title", "status", "detail", "instance"];

/// Stable identifiers for the problems this service reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemType {
    InvalidWorkUrl,
    JobNotFound,
    InvalidJobState,
    AlreadyActive,
    InvalidSession,
    ServiceUnavailable,
    Internal,
}

impl ProblemType {
    /// URNs: stable, collision-free identifiers that do not pretend to be fetchable.
    #[must_use]
    pub const fn uri(self) -> &'static str {
        match self {
            Self::InvalidWorkUrl => "urn:fanfold:problem:invalid-work-url",
            Self::JobNotFound => "urn:fanfold:problem:job-not-found",
            Self::InvalidJobState => "urn:fanfold:problem:invalid-job-state",
            Self::AlreadyActive => "urn:fanfold:problem:already-active",
            Self::InvalidSession => "urn:fanfold:problem:invalid-session",
            Self::ServiceUnavailable => "urn:fanfold:problem:service-unavailable",
            Self::Internal => "urn:fanfold:problem:internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
    /// Sent as the `Retry-After` header, not in the body.
    #[serde(skip)]
    pub retry_after_secs: Option<u64>,
}

impl ProblemDetails {
    /// `about:blank` problem: the status code says it all, and the title is its
    /// reason phrase (RFC 9457 §4.2.1).
    #[must_use]
    pub fn about_blank(status: StatusCode) -> Self {
        Self::new(
            "about:blank",
            status.canonical_reason().unwrap_or("Unknown"),
            status,
        )
    }

    #[must_use]
    pub fn typed(kind: ProblemType, title: &str, status: StatusCode) -> Self {
        Self::new(kind.uri(), title, status)
    }

    fn new(type_uri: &str, title: &str, status: StatusCode) -> Self {
        Self {
            type_uri: type_uri.to_owned(),
            title: title.to_owned(),
            status: status.as_u16(),
            detail: None,
            instance: None,
            extensions: BTreeMap::new(),
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// URI reference identifying this occurrence, e.g. the request path.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Add an extension member. A name that collides with a standard member is
    /// ignored, so an extension can never forge `status` or `type`.
    #[must_use]
    pub fn with_extension(mut self, name: &str, value: Value) -> Self {
        if !RESERVED.contains(&name) {
            self.extensions.insert(name.to_owned(), value);
        }
        self
    }

    #[must_use]
    pub const fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The same response as an axum [`Response`](axum::response::Response).
    #[must_use]
    pub fn into_axum_response(self) -> axum::response::Response {
        let (parts, body) = self.into_http_response().into_parts();
        axum::response::Response::from_parts(parts, axum::body::Body::from(body))
    }

    /// Render as an HTTP response (`application/problem+json`).
    #[must_use]
    pub fn into_http_response(self) -> Response<Vec<u8>> {
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| {
            br#"{"type":"about:blank","title":"Internal Server Error","status":500}"#.to_vec()
        });
        let mut response = Response::new(body);
        *response.status_mut() = self.status_code();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if let Some(secs) = self.retry_after_secs {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

// --------------------------------------------------------------------- mappings

impl From<UrlError> for ProblemDetails {
    fn from(err: UrlError) -> Self {
        Self::typed(
            ProblemType::InvalidWorkUrl,
            "Not an AO3 work URL",
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .with_detail("The value must be a link to an Archive of Our Own work.")
        .with_extension(
            "errors",
            json!([{ "pointer": "#/url", "detail": err.to_string() }]),
        )
    }
}

impl From<&TransitionError> for ProblemDetails {
    fn from(err: &TransitionError) -> Self {
        Self::typed(
            ProblemType::InvalidJobState,
            "Job is in the wrong state",
            StatusCode::CONFLICT,
        )
        .with_detail(err.to_string())
        .with_extension("state", json!(err.from.as_str()))
    }
}

/// Map an application error to a problem.
///
/// Pure on purpose: the caller logs `err` (with its backend detail) and returns
/// this, which carries nothing a client should not see.
impl From<&AppError> for ProblemDetails {
    fn from(err: &AppError) -> Self {
        match err {
            AppError::Repo(fanfold_app::error::RepoError::NotFound(id)) => Self::typed(
                ProblemType::JobNotFound,
                "Job not found",
                StatusCode::NOT_FOUND,
            )
            .with_detail(format!("No job with id {id} exists.")),
            AppError::Repo(fanfold_app::error::RepoError::Conflict(_)) => conflict_problem(),
            AppError::Repo(_) | AppError::Mail(_) | AppError::Store(_) => Self::typed(
                ProblemType::ServiceUnavailable,
                "Service temporarily unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .with_detail("A backing service is not responding. Try again shortly.")
            .with_retry_after(5),
            AppError::Invariant(_) => Self::typed(
                ProblemType::Internal,
                "Internal error",
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .with_detail("The request could not be processed."),
        }
    }
}

fn conflict_problem() -> ProblemDetails {
    ProblemDetails::typed(
        ProblemType::AlreadyActive,
        "Already queued",
        StatusCode::CONFLICT,
    )
    .with_detail("A download of this work is already queued or running.")
}

impl From<&AdminError> for ProblemDetails {
    fn from(err: &AdminError) -> Self {
        match err {
            AdminError::InvalidUrl(e) => (*e).into(),
            AdminError::NotFound(id) => Self::typed(
                ProblemType::JobNotFound,
                "Job not found",
                StatusCode::NOT_FOUND,
            )
            .with_detail(format!("No job with id {id} exists.")),
            AdminError::WrongState(e) => e.into(),
            AdminError::AlreadyActive(_) => conflict_problem(),
            AdminError::Repo(e) => Self::from(&AppError::Repo(e.clone())),
        }
    }
}

impl From<&fanfold_app::library::DeleteError> for ProblemDetails {
    fn from(err: &fanfold_app::library::DeleteError) -> Self {
        use fanfold_app::library::DeleteError;
        match err {
            DeleteError::Busy(_) => {
                Self::about_blank(StatusCode::CONFLICT).with_detail(err.to_string())
            }
            DeleteError::Repo(e) => Self::from(&AppError::Repo(e.clone())),
            DeleteError::Store(_) => Self::about_blank(StatusCode::INTERNAL_SERVER_ERROR)
                .with_detail("The file could not be deleted; the server log has the details."),
        }
    }
}

impl From<&fanfold_app::session::SessionError> for ProblemDetails {
    fn from(err: &fanfold_app::session::SessionError) -> Self {
        use fanfold_app::session::SessionError;
        match err {
            SessionError::Import(_) | SessionError::NeedUserAgent => Self::typed(
                ProblemType::InvalidSession,
                "Cannot use those cookies",
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .with_detail(err.to_string()),
            SessionError::Repo(e) => Self::from(&AppError::Repo(e.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use fanfold_app::error::{MailError, RepoError, StoreError};
    use fanfold_domain::job::{JobId, JobState};
    use proptest::prelude::*;

    use super::*;

    fn body(problem: ProblemDetails) -> Value {
        let response = problem.into_http_response();
        serde_json::from_slice(response.body()).unwrap()
    }

    #[test]
    fn about_blank_uses_the_reason_phrase() {
        assert_eq!(
            body(ProblemDetails::about_blank(StatusCode::NOT_FOUND)),
            json!({"type": "about:blank", "title": "Not Found", "status": 404})
        );
    }

    #[test]
    fn full_document_shape_matches_the_rfc_example() {
        let problem = ProblemDetails::typed(
            ProblemType::InvalidWorkUrl,
            "Bad",
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .with_detail("d")
        .with_instance("/jobs")
        .with_extension("errors", json!([{"pointer": "#/url", "detail": "x"}]));
        assert_eq!(
            body(problem),
            json!({
                "type": "urn:fanfold:problem:invalid-work-url",
                "title": "Bad",
                "status": 422,
                "detail": "d",
                "instance": "/jobs",
                "errors": [{"pointer": "#/url", "detail": "x"}],
            })
        );
    }

    #[test]
    fn response_headers() {
        let response = ProblemDetails::about_blank(StatusCode::SERVICE_UNAVAILABLE)
            .with_retry_after(7)
            .into_http_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let value: Value = serde_json::from_slice(response.body()).unwrap();
        assert!(
            value.get("retry_after_secs").is_none(),
            "retry-after is a header only"
        );
    }

    #[test]
    fn extensions_cannot_shadow_standard_members() {
        let problem = ProblemDetails::about_blank(StatusCode::BAD_REQUEST)
            .with_extension("status", json!(200))
            .with_extension("type", json!("urn:evil"))
            .with_extension("title", json!("fine"))
            .with_extension("detail", json!("x"))
            .with_extension("instance", json!("y"))
            .with_extension("hint", json!("kept"));
        assert_eq!(
            body(problem),
            json!({"type": "about:blank", "title": "Bad Request", "status": 400, "hint": "kept"})
        );
    }

    #[test]
    fn invalid_url_maps_to_422_with_a_pointer() {
        let value = body(UrlError::Host.into());
        assert_eq!(value["status"], 422);
        assert_eq!(value["type"], "urn:fanfold:problem:invalid-work-url");
        assert_eq!(value["errors"][0]["pointer"], "#/url");
        assert_eq!(value["errors"][0]["detail"], "not an AO3 host");
    }

    #[test]
    fn wrong_state_maps_to_409_naming_the_state() {
        let err = TransitionError {
            from: JobState::Running,
            action: "retry",
        };
        let value = body((&err).into());
        assert_eq!(
            (value["status"].clone(), value["state"].clone()),
            (json!(409), json!("running"))
        );
        assert_eq!(value["detail"], "cannot retry a job that is running");
    }

    #[test]
    fn missing_job_maps_to_404() {
        let value = body((&AppError::Repo(RepoError::NotFound(JobId::new(7)))).into());
        assert_eq!(
            (value["status"].clone(), value["detail"].clone()),
            (json!(404), json!("No job with id 7 exists."))
        );
    }

    #[test]
    fn backend_failures_are_503_and_never_leak_details() {
        let secret = "password=hunter2 host=10.0.0.5";
        for err in [
            AppError::Repo(RepoError::Backend(secret.into())),
            AppError::Mail(MailError::Backend(secret.into())),
            AppError::Store(StoreError::Io(secret.into())),
        ] {
            let response = ProblemDetails::from(&err).into_http_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "5");
            let text = String::from_utf8(response.into_body()).unwrap();
            assert!(
                !text.contains("hunter2") && !text.contains("10.0.0.5"),
                "{text}"
            );
        }
    }

    #[test]
    fn invariant_violations_are_a_generic_500() {
        let err = AppError::Invariant(TransitionError {
            from: JobState::Failed,
            action: "start",
        });
        let response = ProblemDetails::from(&err).into_http_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let text = String::from_utf8(response.into_body()).unwrap();
        assert!(
            !text.contains("start") && !text.contains("failed"),
            "internals leaked: {text}"
        );
    }

    proptest! {
        #[test]
        fn always_renders_valid_json_with_a_numeric_status(
            detail in "\\PC*", instance in "\\PC*", key in "\\PC{0,8}", status in 400u16..600,
        ) {
            let status = StatusCode::from_u16(status).unwrap();
            let response = ProblemDetails::about_blank(status)
                .with_detail(detail.clone())
                .with_instance(instance)
                .with_extension(&key, json!(1))
                .into_http_response();
            prop_assert_eq!(response.status(), status);
            let value: Value = serde_json::from_slice(response.body()).unwrap();
            prop_assert_eq!(value["status"].as_u64(), Some(u64::from(status.as_u16())));
            prop_assert_eq!(value["detail"].as_str(), Some(detail.as_str()));
            prop_assert_eq!(value["type"].as_str(), Some("about:blank"));
        }
    }

    #[test]
    fn conflicts_are_409_not_503() {
        let value = body((&AppError::Repo(RepoError::Conflict("secret detail".into()))).into());
        assert_eq!(value["status"], 409);
        assert_eq!(value["type"], "urn:fanfold:problem:already-active");
        assert!(!value.to_string().contains("secret detail"));
    }

    #[test]
    fn admin_errors_map_to_the_right_statuses() {
        use fanfold_domain::work::WorkId;
        let status = |e: AdminError| ProblemDetails::from(&e).status;
        assert_eq!(status(AdminError::InvalidUrl(UrlError::Host)), 422);
        assert_eq!(status(AdminError::NotFound(JobId::new(1))), 404);
        assert_eq!(
            status(AdminError::WrongState(TransitionError {
                from: JobState::Running,
                action: "cancel"
            })),
            409
        );
        assert_eq!(
            status(AdminError::AlreadyActive(WorkId::new(1).unwrap().into())),
            409
        );
        assert_eq!(
            status(AdminError::Repo(RepoError::Backend("x".into()))),
            503
        );
        assert_eq!(
            status(AdminError::Repo(RepoError::Conflict("x".into()))),
            409
        );
    }
}
