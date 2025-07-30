use actix_files::Files;
use actix_web::{web, App, HttpServer};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            .service(Files::new("/", "./static").index_file("index.html"))
            .route("/ws", web::get().to(ws_route))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}