use crate::{cloud, AuthData};
use arroyo_server_common::log_event;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, TypedHeader};
use deadpool_postgres::{Object, Pool};
use serde_json::json;
use thiserror::Error;
use tracing::error;

use axum::headers::authorization::{Authorization, Bearer};
use tonic::Code;

pub type BearerAuth = Option<TypedHeader<Authorization<Bearer>>>;

pub struct ErrorResp {
    pub(crate) status_code: StatusCode,
    pub(crate) message: String,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    JsonExtractorRejection(#[from] JsonRejection),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ApiError::JsonExtractorRejection(json_rejection) => {
                (json_rejection.status(), json_rejection.body_text())
            }
        };

        ErrorResp {
            status_code: status,
            message,
        }
        .into_response()
    }
}

impl From<tonic::Status> for ErrorResp {
    fn from(value: tonic::Status) -> Self {
        let status_code = match value.code() {
            Code::Cancelled => StatusCode::REQUEST_TIMEOUT,
            Code::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
            Code::InvalidArgument => StatusCode::BAD_REQUEST,
            Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
            Code::NotFound => StatusCode::NOT_FOUND,
            Code::AlreadyExists => StatusCode::CONFLICT,
            Code::PermissionDenied => StatusCode::FORBIDDEN,
            Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            Code::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
            Code::Aborted => StatusCode::CONFLICT,
            Code::OutOfRange => StatusCode::BAD_REQUEST,
            Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
            Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
            Code::Unauthenticated => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        ErrorResp {
            status_code,
            message: value.message().to_string(),
        }
    }
}

pub fn log_and_map_rest<E>(err: E) -> ErrorResp
where
    E: core::fmt::Debug,
{
    error!("Error while handling: {:?}", err);
    log_event("api_error", json!({ "error": format!("{:?}", err) }));
    ErrorResp {
        status_code: StatusCode::INTERNAL_SERVER_ERROR,
        message: "Something went wrong".to_string(),
    }
}

impl IntoResponse for ErrorResp {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": self.message,
        }));
        (self.status_code, body).into_response()
    }
}

pub async fn client(pool: &Pool) -> Result<Object, ErrorResp> {
    pool.get().await.map_err(log_and_map_rest)
}

pub(crate) async fn authenticate(
    pool: &Pool,
    bearer_auth: BearerAuth,
) -> Result<AuthData, ErrorResp> {
    let client = client(pool).await?;
    cloud::authenticate_rest(client, bearer_auth).await
}
