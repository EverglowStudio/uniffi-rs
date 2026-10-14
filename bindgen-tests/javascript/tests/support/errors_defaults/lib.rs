uniffi::setup_scaffolding!();

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum ErrorCode {
    Network,
    InvalidArgument,
}

#[derive(Clone, Debug, PartialEq, uniffi::Error)]
pub enum SdkError {
    Failure {
        code: ErrorCode,
        message: String,
        retryable: bool,
        message_id: Option<String>,
    },
    Rejected {
        reason: String,
    },
    Closed,
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SdkError {}

#[derive(Clone, Debug, PartialEq, uniffi::Error)]
pub enum UnitError {
    Offline,
    Closed,
}
impl std::fmt::Display for UnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for UnitError {}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ErrorState {
    pub error: SdkError,
    pub optional_error: Option<SdkError>,
    pub unit_error: UnitError,
    pub optional_unit: Option<UnitError>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct Defaults {
    pub required: String,
    #[uniffi(default = 7)]
    pub count: u32,
    #[uniffi(default = Some("record-default"))]
    pub label: Option<String>,
    #[uniffi(default = None)]
    pub empty: Option<String>,
}

#[uniffi::export]
pub fn inspect_defaults(value: Defaults) -> String {
    format!(
        "{}|{}|{}|{}",
        value.required,
        value.count,
        value.label.as_deref().unwrap_or("<none>"),
        value.empty.as_deref().unwrap_or("<none>")
    )
}

#[uniffi::export(default(count = 11, label = Some("argument-default")))]
pub fn inspect_arguments(required: String, count: u32, label: Option<String>) -> String {
    inspect_defaults(Defaults {
        required,
        count,
        label,
        empty: None,
    })
}

fn failure() -> SdkError {
    SdkError::Failure {
        code: ErrorCode::Network,
        message: "network unavailable".into(),
        retryable: true,
        message_id: Some("m-1".into()),
    }
}

#[uniffi::export]
pub fn fail(kind: u32) -> Result<(), SdkError> {
    Err(match kind {
        0 => failure(),
        1 => SdkError::Rejected {
            reason: "invalid".into(),
        },
        _ => SdkError::Closed,
    })
}

#[uniffi::export]
pub fn fail_unit() -> Result<(), UnitError> {
    Err(UnitError::Offline)
}

#[uniffi::export]
pub fn error_state() -> ErrorState {
    ErrorState {
        error: failure(),
        optional_error: Some(SdkError::Closed),
        unit_error: UnitError::Offline,
        optional_unit: Some(UnitError::Closed),
    }
}

#[uniffi::export]
pub fn echo_errors(value: ErrorState) -> ErrorState {
    value
}

// This Rust comparison proves that a JS/native value survived lowering,
// rather than merely comparing two views of the same JS object.
#[uniffi::export]
pub fn is_original(value: ErrorState) -> bool {
    value == error_state()
}
