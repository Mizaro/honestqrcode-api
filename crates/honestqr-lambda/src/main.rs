use axum::body::Body as AxumBody;
use honestqr_http::{AppConfig, try_router};
use http_body_util::BodyExt;
use lambda_http::{Body, Error, Request, Response, run, service_fn};
use tower::ServiceExt;

#[tokio::main]
async fn main() -> Result<(), Error> {
    lambda_http::tracing::init_default_subscriber();
    let app = try_router(AppConfig::default()).map_err(|error| Error::from(error.to_string()))?;
    run(service_fn(move |request| {
        let app = app.clone();
        async move { handle(app, request).await }
    }))
    .await
}

async fn handle(app: axum::Router, request: Request) -> Result<Response<Body>, Error> {
    let (parts, body) = request.into_parts();
    let body = match body {
        Body::Empty => AxumBody::empty(),
        Body::Text(text) => AxumBody::from(text),
        Body::Binary(bytes) => AxumBody::from(bytes),
        _ => AxumBody::empty(),
    };
    let request = axum::http::Request::from_parts(parts, body);
    let response = app.oneshot(request).await?;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await?.to_bytes();
    Ok(Response::from_parts(parts, Body::Binary(bytes.into())))
}

#[cfg(test)]
mod tests {
    #[test]
    fn platform_timeout_exceeds_the_router_deadline() {
        let template = include_str!("../../../deploy/aws-lambda/template.yaml");
        let platform_timeout = template
            .lines()
            .find_map(|line| line.trim().strip_prefix("Timeout: "))
            .and_then(|value| value.parse::<u64>().ok())
            .expect("SAM template has a numeric function timeout");

        assert!(
            platform_timeout > honestqr_http::DEFAULT_REQUEST_TIMEOUT_SECONDS,
            "Lambda must leave headroom above the router timeout"
        );
    }
}
