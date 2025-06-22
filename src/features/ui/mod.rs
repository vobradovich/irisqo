use crate::models::AppState;
use askama::Template;
use askama_web::WebTemplate;
use axum::{Router, routing::get};
use std::sync::Arc;

#[derive(Template, WebTemplate)]
#[template(path = "ui/ui.html")]
struct UiTemplate;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new().route("/ui", get(ui)).with_state(state)
}

async fn ui() -> UiTemplate {
    UiTemplate
}
