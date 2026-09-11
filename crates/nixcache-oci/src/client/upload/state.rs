use crate::error::{OciError, TransportError};
use http::StatusCode;

pub(super) fn invalid_upload_range(details: impl Into<String>) -> OciError {
    OciError::UploadRangeInvalid {
        details: details.into(),
    }
}

pub(super) fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

pub(super) fn retryable_error(error: &OciError) -> bool {
    match error {
        OciError::BlobUploadFailed(status) => retryable_status(*status),
        OciError::Transport(TransportError::Io(_))
        | OciError::Transport(TransportError::ConnectionFailed { .. })
        | OciError::Transport(TransportError::Timeout { .. }) => true,
        OciError::Transport(TransportError::HttpStatus { status, .. }) => retryable_status(*status),
        _ => false,
    }
}

pub(super) fn response_next_offset(
    range: Option<(u64, u64)>,
    chunk_start: u64,
    request_start: u64,
    chunk_end: u64,
) -> Result<u64, OciError> {
    let Some((start, end)) = range else {
        return chunk_end
            .checked_add(1)
            .ok_or_else(|| invalid_upload_range("chunk end offset overflowed"));
    };
    if start != 0 && start != chunk_start && start != request_start {
        return Err(invalid_upload_range(format!(
            "range starts at {start}, expected 0, chunk start {chunk_start}, or request start {request_start}"
        )));
    }
    if end < request_start {
        return Err(invalid_upload_range(format!(
            "range ends at {end}, before requested offset {request_start}"
        )));
    }
    if end > chunk_end {
        return Err(invalid_upload_range(format!(
            "range ends at {end}, beyond chunk end {chunk_end}"
        )));
    }
    end.checked_add(1)
        .ok_or_else(|| invalid_upload_range("range end offset overflowed"))
}

pub(super) fn probe_next_offset(
    range: Option<(u64, u64)>,
    chunk_start: u64,
    current_offset: u64,
    chunk_end: u64,
) -> Result<Option<u64>, OciError> {
    let Some((start, end)) = range else {
        return Ok(None);
    };
    if start != 0 && start != chunk_start {
        return Err(invalid_upload_range(format!(
            "probe range starts at {start}, expected 0 or chunk start {chunk_start}"
        )));
    }
    if end > chunk_end {
        return Err(invalid_upload_range(format!(
            "probe range ends at {end}, beyond chunk end {chunk_end}"
        )));
    }
    let next = end
        .checked_add(1)
        .ok_or_else(|| invalid_upload_range("probe range end offset overflowed"))?;
    if next < current_offset {
        return Err(invalid_upload_range(format!(
            "probe moved remote offset backwards from {current_offset} to {next}"
        )));
    }
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::{probe_next_offset, response_next_offset};

    #[test]
    fn response_offset_accepts_missing_range() {
        assert_eq!(response_next_offset(None, 0, 0, 9).unwrap(), 10);
    }

    #[test]
    fn probe_offset_rejects_backwards_progress() {
        assert!(probe_next_offset(Some((0, 1)), 0, 3, 9).is_err());
    }
}
