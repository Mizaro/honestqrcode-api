use axum::body::Body as AxumBody;
use honestqr_http::{AppConfig, router};
use http_body_util::BodyExt;
use lambda_http::{Body, Error, Request, Response, run, service_fn};
use tower::ServiceExt;

#[tokio::main]
async fn main() -> Result<(), Error> {
    lambda_http::tracing::init_default_subscriber();
    let app = router(AppConfig::default());
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
    Ok(Response::from_parts(parts, Body::Binary(bytes.to_vec())))
}
