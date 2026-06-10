use axum::http::StatusCode;
use garde::Validate;

pub fn validate_or_bad_request<T>(value: &T, message: &'static str) -> Result<(), StatusCode>
where
    T: Validate<Context = ()>,
{
    value.validate().map_err(|err| {
        tracing::debug!("Validation failed: {message}: {err}");
        StatusCode::BAD_REQUEST
    })
}

pub fn validate_or_bad_request_text<T>(
    value: &T,
    message: &'static str,
) -> Result<(), (StatusCode, String)>
where
    T: Validate<Context = ()>,
{
    validate_or_bad_request(value, message).map_err(|status| (status, message.to_string()))
}

pub fn trimmed_non_empty(value: &str, _: &()) -> garde::Result {
    if value.trim().is_empty() {
        Err(garde::Error::new("must not be empty"))
    } else {
        Ok(())
    }
}

pub fn no_control_chars(value: &str, _: &()) -> garde::Result {
    if value.chars().any(char::is_control) {
        Err(garde::Error::new("must not contain control characters"))
    } else {
        Ok(())
    }
}

pub fn optional_tag_list(value: &Option<Vec<String>>, _: &()) -> garde::Result {
    if let Some(tags) = value {
        if tags.len() > 32 {
            return Err(garde::Error::new("must not contain more than 32 tags"));
        }
        for tag in tags {
            trimmed_non_empty(tag, &())?;
            no_control_chars(tag, &())?;
            if tag.len() > 64 {
                return Err(garde::Error::new("tag must not exceed 64 characters"));
            }
        }
    }
    Ok(())
}

pub fn optional_uuid_list(value: &Option<Vec<String>>, _: &()) -> garde::Result {
    if let Some(ids) = value {
        if ids.len() > 64 {
            return Err(garde::Error::new("must not contain more than 64 ids"));
        }
        for id in ids {
            trimmed_non_empty(id, &())?;
            no_control_chars(id, &())?;
            uuid::Uuid::parse_str(id).map_err(|_| garde::Error::new("must be a valid UUID"))?;
        }
    }
    Ok(())
}

pub fn safe_resource_id(value: &str, _: &()) -> garde::Result {
    if value.trim() != value {
        return Err(garde::Error::new("must not have surrounding whitespace"));
    }
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        return Err(garde::Error::new("contains an invalid path segment"));
    }
    Ok(())
}

#[derive(garde::Validate)]
#[garde(transparent)]
struct ResourceIdRef<'a>(
    #[garde(
        length(min = 1, max = 64),
        custom(trimmed_non_empty),
        custom(no_control_chars),
        custom(safe_resource_id)
    )]
    &'a str,
);

pub fn validate_resource_id(id: &str) -> Result<(), StatusCode> {
    validate_or_bad_request(&ResourceIdRef(id), "Invalid resource ID")
}
