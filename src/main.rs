use actix::{Actor, Addr};
use actix_files::Files;
use actix_web::{web, App, Error, HttpServer, middleware::Logger};
use actix_web_actors::ws;
use std::sync::Arc;

mod coordinator;
mod models;
mod websocket;

use coordinator::Coordinator;
use websocket::WebSocketSession;

async fn ws_route(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    coordinator: web::Data<Arc<Addr<Coordinator>>>,
) -> Result<actix_web::HttpResponse, Error> {
    let client_addr = req.peer_addr();
    
    ws::start(
        WebSocketSession::new(coordinator.get_ref().as_ref().clone(), client_addr),
        &req,
        stream,
    )
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    
    let coordinator = Coordinator::new().start();
    let coordinator_data = Arc::new(coordinator);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(web::Data::new(coordinator_data.clone()))
            .service(Files::new("/", "./static").index_file("index.html"))
            .route("/ws", web::get().to(ws_route))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}